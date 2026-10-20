//! Streaming parser for IMAP `UID SEARCH` command responses.
//!
//! IMAP sends a search result as one untagged `* SEARCH` line. Some mailboxes
//! legitimately make that line much larger than a normal command transcript,
//! so this module reads directly from the buffered transport and never builds
//! a line or response transcript in memory.

use anyhow::{bail, Result};
use std::future::Future;
use tokio::io::{AsyncBufRead, AsyncBufReadExt};

/// Largest page accepted by this parser. A page is the only UID collection
/// retained in process memory while a response is being read.
pub const MAX_SEARCH_UID_PAGE_SIZE: usize = 10_000;

/// Fixed page sizing for a streamed search result.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SearchUidPageLimits {
    /// Number of UIDs supplied to the page callback at a time.
    pub page_size: usize,
}

impl SearchUidPageLimits {
    pub fn validate(self) -> Result<()> {
        if self.page_size == 0 {
            bail!("IMAP search page size must be greater than zero");
        }
        if self.page_size > MAX_SEARCH_UID_PAGE_SIZE {
            bail!("IMAP search page size exceeds the fixed parser limit");
        }
        Ok(())
    }
}

/// Completion status from the command's matching tagged response.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchUidCompletion {
    Ok,
    No,
}

/// Counts collected while parsing one `UID SEARCH` response.
///
/// Pages are delivered to the callback before the tagged completion is known.
/// A caller must stage them as incomplete and mark that staging authoritative
/// only when `completion` is [`SearchUidCompletion::Ok`]. A tagged `NO` or an
/// error, including EOF, must leave the staged discovery incomplete.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SearchUidMetrics {
    pub completion: SearchUidCompletion,
    pub uid_count: usize,
    pub page_count: usize,
    /// Largest UID page retained while parsing this command.
    pub max_retained_uids: usize,
    /// Byte footprint of the retained `u32` UID values alone. This does not
    /// claim process-memory usage or buffered socket capacity.
    pub max_retained_uid_bytes: usize,
}

/// Consume an IMAP `UID SEARCH` response and stream fixed-size UID pages.
///
/// `expected_tag` must be the tag written for this command. Exactly one
/// untagged `* SEARCH` result is accepted. Every UID is parsed as a non-zero
/// `u32`; malformed values and duplicate SEARCH responses fail closed.
///
/// The callback is deliberately invoked before the matching tagged response:
/// it lets callers persist pages in temporary storage without retaining a
/// whole mailbox in RAM. The callback must not publish or reconcile the data
/// until this function returns `SearchUidCompletion::Ok`.
pub async fn parse_uid_search_response<R, F, Fut>(
    reader: &mut R,
    expected_tag: &[u8],
    limits: SearchUidPageLimits,
    mut on_page: F,
) -> Result<SearchUidMetrics>
where
    R: AsyncBufRead + Unpin,
    F: FnMut(Vec<u32>) -> Fut,
    Fut: Future<Output = Result<()>>,
{
    limits.validate()?;
    if expected_tag.is_empty() || expected_tag.iter().any(|byte| !is_atom_byte(*byte)) {
        bail!("IMAP search command tag is malformed");
    }

    let mut search_seen = false;
    let mut page = Vec::with_capacity(limits.page_size);
    let mut uid_count = 0usize;
    let mut page_count = 0usize;
    let mut max_retained_uids = 0usize;

    loop {
        let first = read_byte(reader)
            .await?
            .ok_or_else(|| anyhow::anyhow!("IMAP connection closed before search completion"))?;

        if first == b'*' {
            match read_byte(reader).await? {
                Some(b' ') => {}
                Some(_) => bail!("IMAP untagged response is malformed"),
                None => bail!("IMAP connection closed during untagged response"),
            }

            let kind = read_atom(reader, None).await?;
            if kind.is_search {
                if search_seen {
                    bail!("IMAP response contains multiple SEARCH results");
                }
                search_seen = true;
                parse_search_values(
                    reader,
                    kind.ended_line,
                    limits,
                    &mut page,
                    &mut uid_count,
                    &mut page_count,
                    &mut max_retained_uids,
                    &mut on_page,
                )
                .await?;
            } else if !kind.ended_line {
                skip_line(reader).await?;
            }
            continue;
        }

        if first == b'+' {
            skip_line(reader).await?;
            continue;
        }

        let tag = read_atom_after_first(reader, first, Some(expected_tag)).await?;
        if !tag.matches_expected {
            if !tag.ended_line {
                skip_line(reader).await?;
            }
            continue;
        }
        if tag.ended_line {
            bail!("IMAP tagged search response omitted a status");
        }

        let status = read_atom(reader, None).await?;
        let completion = if status.is_ok {
            SearchUidCompletion::Ok
        } else if status.is_no {
            SearchUidCompletion::No
        } else {
            bail!("IMAP tagged search response has unsupported status");
        };
        if !status.ended_line {
            skip_line(reader).await?;
        }

        if completion == SearchUidCompletion::Ok && !search_seen {
            bail!("IMAP tagged search response omitted SEARCH result");
        }
        return Ok(SearchUidMetrics {
            completion,
            uid_count,
            page_count,
            max_retained_uids,
            max_retained_uid_bytes: max_retained_uids.saturating_mul(std::mem::size_of::<u32>()),
        });
    }
}

#[derive(Debug, Clone, Copy)]
struct Atom {
    ended_line: bool,
    matches_expected: bool,
    is_search: bool,
    is_ok: bool,
    is_no: bool,
}

async fn read_atom<R>(reader: &mut R, expected: Option<&[u8]>) -> Result<Atom>
where
    R: AsyncBufRead + Unpin,
{
    let first = read_byte(reader)
        .await?
        .ok_or_else(|| anyhow::anyhow!("IMAP connection closed during response atom"))?;
    read_atom_after_first(reader, first, expected).await
}

async fn read_atom_after_first<R>(
    reader: &mut R,
    first: u8,
    expected: Option<&[u8]>,
) -> Result<Atom>
where
    R: AsyncBufRead + Unpin,
{
    if !is_atom_byte(first) {
        bail!("IMAP response contains a malformed atom");
    }

    let mut len = 0usize;
    let mut matches_expected = expected.is_some();
    let mut is_search = true;
    let mut is_ok = true;
    let mut is_no = true;
    let mut current = first;

    loop {
        matches_expected &= expected
            .map(|value| len < value.len() && ascii_eq(current, value[len]))
            .unwrap_or(false);
        is_search &= len < b"SEARCH".len() && ascii_eq(current, b"SEARCH"[len]);
        is_ok &= len < b"OK".len() && ascii_eq(current, b"OK"[len]);
        is_no &= len < b"NO".len() && ascii_eq(current, b"NO"[len]);
        len = len
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("IMAP response atom is too long"))?;

        let next = read_byte(reader)
            .await?
            .ok_or_else(|| anyhow::anyhow!("IMAP connection closed during response atom"))?;
        match next {
            b' ' => {
                return Ok(Atom {
                    ended_line: false,
                    matches_expected: matches_expected
                        && expected.is_some_and(|value| value.len() == len),
                    is_search: is_search && len == b"SEARCH".len(),
                    is_ok: is_ok && len == b"OK".len(),
                    is_no: is_no && len == b"NO".len(),
                });
            }
            b'\r' => {
                expect_lf(reader).await?;
                return Ok(Atom {
                    ended_line: true,
                    matches_expected: matches_expected
                        && expected.is_some_and(|value| value.len() == len),
                    is_search: is_search && len == b"SEARCH".len(),
                    is_ok: is_ok && len == b"OK".len(),
                    is_no: is_no && len == b"NO".len(),
                });
            }
            byte if is_atom_byte(byte) => current = byte,
            _ => bail!("IMAP response contains a malformed atom"),
        }
    }
}

async fn parse_search_values<R, F, Fut>(
    reader: &mut R,
    ended_line: bool,
    limits: SearchUidPageLimits,
    page: &mut Vec<u32>,
    uid_count: &mut usize,
    page_count: &mut usize,
    max_retained_uids: &mut usize,
    on_page: &mut F,
) -> Result<()>
where
    R: AsyncBufRead + Unpin,
    F: FnMut(Vec<u32>) -> Fut,
    Fut: Future<Output = Result<()>>,
{
    if ended_line {
        return Ok(());
    }

    let mut value = 0u32;
    let mut digits = 0usize;
    loop {
        let byte = read_byte(reader)
            .await?
            .ok_or_else(|| anyhow::anyhow!("IMAP connection closed during SEARCH result"))?;
        match byte {
            b'0'..=b'9' => {
                digits = digits
                    .checked_add(1)
                    .ok_or_else(|| anyhow::anyhow!("IMAP SEARCH UID is too long"))?;
                value = value
                    .checked_mul(10)
                    .and_then(|number| number.checked_add(u32::from(byte - b'0')))
                    .ok_or_else(|| anyhow::anyhow!("IMAP SEARCH UID is out of range"))?;
            }
            b' ' | b'\r' => {
                if digits != 0 {
                    if value == 0 {
                        bail!("IMAP SEARCH result contains UID 0");
                    }
                    page.push(value);
                    *uid_count = uid_count.checked_add(1).ok_or_else(|| {
                        anyhow::anyhow!("IMAP SEARCH result contains too many UIDs")
                    })?;
                    value = 0;
                    digits = 0;
                    if page.len() == limits.page_size {
                        emit_page(page, page_count, max_retained_uids, on_page).await?;
                    }
                }
                if byte == b'\r' {
                    expect_lf(reader).await?;
                    if !page.is_empty() {
                        emit_page(page, page_count, max_retained_uids, on_page).await?;
                    }
                    return Ok(());
                }
            }
            _ => bail!("IMAP SEARCH result contains a malformed UID"),
        }
    }
}

async fn emit_page<F, Fut>(
    page: &mut Vec<u32>,
    page_count: &mut usize,
    max_retained_uids: &mut usize,
    on_page: &mut F,
) -> Result<()>
where
    F: FnMut(Vec<u32>) -> Fut,
    Fut: Future<Output = Result<()>>,
{
    let completed_page = std::mem::take(page);
    *max_retained_uids = (*max_retained_uids).max(completed_page.len());
    on_page(completed_page).await?;
    *page_count = page_count
        .checked_add(1)
        .ok_or_else(|| anyhow::anyhow!("IMAP SEARCH result contains too many pages"))?;
    Ok(())
}

async fn skip_line<R>(reader: &mut R) -> Result<()>
where
    R: AsyncBufRead + Unpin,
{
    loop {
        let byte = read_byte(reader)
            .await?
            .ok_or_else(|| anyhow::anyhow!("IMAP connection closed during response line"))?;
        if byte == b'\r' {
            expect_lf(reader).await?;
            return Ok(());
        }
    }
}

async fn expect_lf<R>(reader: &mut R) -> Result<()>
where
    R: AsyncBufRead + Unpin,
{
    match read_byte(reader).await? {
        Some(b'\n') => Ok(()),
        _ => bail!("IMAP response is not CRLF terminated"),
    }
}

async fn read_byte<R>(reader: &mut R) -> Result<Option<u8>>
where
    R: AsyncBufRead + Unpin,
{
    let buffer = reader.fill_buf().await?;
    let Some(byte) = buffer.first().copied() else {
        return Ok(None);
    };
    reader.consume(1);
    Ok(Some(byte))
}

fn is_atom_byte(byte: u8) -> bool {
    (0x21..=0x7e).contains(&byte)
        && !matches!(
            byte,
            b'(' | b')' | b'{' | b' ' | b'%' | b'*' | b'"' | b'\\' | b']'
        )
}

fn ascii_eq(actual: u8, expected: u8) -> bool {
    actual.eq_ignore_ascii_case(&expected)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    use tokio::io::BufReader;

    const LIMITS: SearchUidPageLimits = SearchUidPageLimits { page_size: 2 };

    async fn parse(transcript: &str) -> Result<SearchUidMetrics> {
        let mut reader = BufReader::new(Cursor::new(transcript.as_bytes().to_vec()));
        parse_uid_search_response(&mut reader, b"D0001", LIMITS, |_| async { Ok(()) }).await
    }

    #[tokio::test]
    async fn streams_incremental_search_into_fixed_pages_before_tagged_ok() {
        let transcript =
            "* OK mailbox ready\r\n* SEARCH 4 8 15 16 23\r\nD0001 OK SEARCH complete\r\n";
        let mut reader = BufReader::new(Cursor::new(transcript.as_bytes().to_vec()));
        let mut staged = Vec::new();
        let metrics = parse_uid_search_response(&mut reader, b"D0001", LIMITS, |page| {
            staged.push(page);
            async { Ok(()) }
        })
        .await
        .unwrap();

        assert_eq!(metrics.completion, SearchUidCompletion::Ok);
        assert_eq!(metrics.uid_count, 5);
        assert_eq!(metrics.page_count, 3);
        assert_eq!(metrics.max_retained_uids, 2);
        assert_eq!(metrics.max_retained_uid_bytes, 8);
        assert_eq!(staged, vec![vec![4, 8], vec![15, 16], vec![23]]);
        assert!(staged.iter().all(|page| page.len() <= LIMITS.page_size));
    }

    #[tokio::test]
    async fn accepts_empty_search_result() {
        let metrics = parse("* SEARCH\r\nD0001 OK completed\r\n").await.unwrap();
        assert_eq!(metrics.completion, SearchUidCompletion::Ok);
        assert_eq!(metrics.uid_count, 0);
        assert_eq!(metrics.page_count, 0);
    }

    #[tokio::test]
    async fn tagged_no_reports_incomplete_staging_without_retaining_pages() {
        let transcript = "* SEARCH 4 8\r\nD0001 NO rejected\r\n";
        let mut reader = BufReader::new(Cursor::new(transcript.as_bytes().to_vec()));
        let mut staged = Vec::new();
        let metrics = parse_uid_search_response(&mut reader, b"D0001", LIMITS, |page| {
            staged.push(page);
            async { Ok(()) }
        })
        .await
        .unwrap();

        assert_eq!(metrics.completion, SearchUidCompletion::No);
        assert_eq!(metrics.uid_count, 2);
        assert_eq!(staged, vec![vec![4, 8]]);
    }

    #[tokio::test]
    async fn rejects_malformed_or_zero_uids_and_multiple_searches() {
        for transcript in [
            "* SEARCH 4 invalid 8\r\nD0001 OK complete\r\n",
            "* SEARCH 0\r\nD0001 OK complete\r\n",
            "* SEARCH 4\r\n* SEARCH 8\r\nD0001 OK complete\r\n",
        ] {
            assert!(parse(transcript).await.is_err(), "{transcript:?}");
        }
    }

    #[tokio::test]
    async fn enforces_fixed_page_limit_and_never_needs_a_full_search_line() {
        let values = (1..=1_001)
            .map(|value| value.to_string())
            .collect::<Vec<_>>()
            .join(" ");
        let transcript = format!("* SEARCH {values}\r\nD0001 OK complete\r\n");
        let mut reader = BufReader::with_capacity(3, Cursor::new(transcript.into_bytes()));
        let mut largest_page = 0usize;
        let metrics = parse_uid_search_response(
            &mut reader,
            b"D0001",
            SearchUidPageLimits { page_size: 17 },
            |page| {
                largest_page = largest_page.max(page.len());
                async { Ok(()) }
            },
        )
        .await
        .unwrap();

        assert_eq!(metrics.uid_count, 1_001);
        assert_eq!(metrics.page_count, 59);
        assert_eq!(metrics.max_retained_uids, 17);
        assert_eq!(metrics.max_retained_uid_bytes, 68);
        assert_eq!(largest_page, 17);
    }

    #[tokio::test]
    async fn rejects_bad_or_missing_tagged_completion() {
        for transcript in [
            "* SEARCH 4\r\nD0001 BAD invalid\r\n",
            "* SEARCH 4\r\nD0001 OK complete",
        ] {
            assert!(parse(transcript).await.is_err(), "{transcript:?}");
        }
    }
}
