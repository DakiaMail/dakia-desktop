use chrono::NaiveDate;
use dakia_core::{
    compile_generic_imap, compile_sql_candidate, evaluate_search, parse_search_query,
    search::{
        SearchExpression, SearchField, SearchNode, SearchParseErrorKind, SearchTerm, SearchText,
    },
    search_imap::ImapCompileErrorKind,
    search_session::{
        ProviderSearchCursor, SearchContinuationError, SearchContinuationV2, SearchExecutionMode,
        SearchRequestV2, SearchScopeV2, SearchSessionRegistry,
    },
    search_sql::{SqlSearchBind, SqlSearchCompileErrorKind},
    SearchableMessage,
};
use uuid::Uuid;

fn today() -> NaiveDate {
    NaiveDate::from_ymd_opt(2026, 9, 6).unwrap()
}

fn request(query: &str) -> SearchRequestV2 {
    SearchRequestV2 {
        raw_query: query.to_owned(),
        client_request_id: None,
        account_ids: vec![Uuid::from_u128(1), Uuid::from_u128(2)],
        scope: SearchScopeV2 {
            mailbox: Some("Archive".into()),
            include_spam_trash: false,
        },
        execution_mode: SearchExecutionMode::Hybrid,
        page_size: 50,
        continuation: None,
    }
}

#[test]
fn malformed_positions_are_utf8_byte_offsets_at_character_boundaries() {
    let input = "Åsa OR )";
    let error = parse_search_query(input).unwrap_err();
    assert_eq!(error.position, "Åsa OR".len());
    assert!(input.is_char_boundary(error.position));
    assert_eq!(error.kind, SearchParseErrorKind::MissingOperand);

    let input = "é body:ok\0OR ALL";
    let error = parse_search_query(input).unwrap_err();
    assert_eq!(error.position, "é body:ok".len());
    assert!(input.is_char_boundary(error.position));
    assert_eq!(error.kind, SearchParseErrorKind::ControlCharacter);
}

#[test]
fn boolean_evaluation_obeys_complement_and_demorgan_for_real_messages() {
    let messages = [
        ("Alice Example <alice@example.test>", "quarterly invoice"),
        ("Bob Example <bob@example.test>", "quarterly update"),
        ("Carol Example <carol@example.test>", "travel receipt"),
    ];
    let a = parse_search_query("from:alice").unwrap();
    let b = parse_search_query("subject:quarterly").unwrap();
    let not_a = parse_search_query("NOT from:alice").unwrap();
    let demorgan_left = parse_search_query("NOT (from:alice OR subject:quarterly)").unwrap();
    let demorgan_right = parse_search_query("NOT from:alice AND NOT subject:quarterly").unwrap();

    for (from, subject) in messages {
        let message = SearchableMessage {
            from,
            to: "",
            cc: "",
            bcc: "",
            subject,
            body: "",
            mailboxes: &[],
            mailbox: "INBOX",
            received_on: today(),
            attachments: &[],
            is_read: false,
            is_flagged: false,
            is_replied: false,
            is_draft: false,
        };
        let a_value = evaluate_search(&a, &message, today());
        let b_value = evaluate_search(&b, &message, today());
        assert_eq!(evaluate_search(&not_a, &message, today()), !a_value);
        assert_eq!(
            evaluate_search(&demorgan_left, &message, today()),
            !a_value && !b_value
        );
        assert_eq!(
            evaluate_search(&demorgan_right, &message, today()),
            !a_value && !b_value
        );
    }
}

#[test]
fn generated_queries_round_trip_and_preserve_independent_boolean_semantics() {
    for seed in [1, 7, 73, 7331, 0x51f15e, 0xc0ffee, u32::MAX] {
        let mut random = SearchXorShift32(seed);
        for case in 0..128 {
            let generated = GeneratedSearch::new(&mut random, 0);
            let raw = generated.render();
            let parsed = parse_search_query(&raw).unwrap_or_else(|error| {
                panic!("seed={seed} case={case} generated invalid {raw:?}: {error}")
            });
            let rendered = render_parsed_node(&parsed.root);
            assert_eq!(
                parse_search_query(&rendered).unwrap(),
                parsed,
                "seed={seed} case={case} raw={raw:?} rendered={rendered:?}"
            );

            let message = GeneratedMessage::new(&mut random);
            let searchable = message.as_searchable();
            assert_eq!(
                evaluate_search(&parsed, &searchable, today()),
                generated.evaluate(&message),
                "seed={seed} case={case} query={raw:?} message={message:?}"
            );
        }
    }
}

#[test]
fn generated_precedence_and_demorgan_invariants_hold_for_real_evaluation() {
    for seed in 1..=512 {
        let mut random = SearchXorShift32(seed);
        let a = GeneratedSearch::leaf(&mut random);
        let b = GeneratedSearch::leaf(&mut random);
        let c = GeneratedSearch::leaf(&mut random);
        let message = GeneratedMessage::new(&mut random);
        let searchable = message.as_searchable();

        let precedence = format!("{} OR {} AND {}", a.render(), b.render(), c.render());
        let explicit = format!("{} OR ({} AND {})", a.render(), b.render(), c.render());
        assert_eq!(
            evaluate_search(
                &parse_search_query(&precedence).unwrap(),
                &searchable,
                today()
            ),
            evaluate_search(
                &parse_search_query(&explicit).unwrap(),
                &searchable,
                today()
            ),
            "seed={seed} precedence={precedence:?}"
        );

        let left = format!("NOT ({} OR {})", a.render(), b.render());
        let right = format!("NOT {} AND NOT {}", a.render(), b.render());
        assert_eq!(
            evaluate_search(&parse_search_query(&left).unwrap(), &searchable, today()),
            evaluate_search(&parse_search_query(&right).unwrap(), &searchable, today()),
            "seed={seed} de_morgan_left={left:?} de_morgan_right={right:?}"
        );
    }
}

#[derive(Debug, Clone)]
enum GeneratedSearch {
    Leaf {
        field: GeneratedField,
        word: &'static str,
    },
    And(Box<Self>, Box<Self>),
    Or(Box<Self>, Box<Self>),
    Not(Box<Self>),
}

#[derive(Debug, Clone, Copy)]
enum GeneratedField {
    Anywhere,
    From,
    Subject,
    Body,
}

impl GeneratedSearch {
    fn new(random: &mut SearchXorShift32, depth: usize) -> Self {
        if depth >= 3 {
            return Self::leaf(random);
        }
        match random.next_usize(4) {
            0 => Self::leaf(random),
            1 => Self::And(
                Box::new(Self::new(random, depth + 1)),
                Box::new(Self::new(random, depth + 1)),
            ),
            2 => Self::Or(
                Box::new(Self::new(random, depth + 1)),
                Box::new(Self::new(random, depth + 1)),
            ),
            _ => Self::Not(Box::new(Self::new(random, depth + 1))),
        }
    }

    fn leaf(random: &mut SearchXorShift32) -> Self {
        const WORDS: &[&str] = &["alpha", "bravo", "charlie", "delta", "echo"];
        let field = match random.next_usize(4) {
            0 => GeneratedField::Anywhere,
            1 => GeneratedField::From,
            2 => GeneratedField::Subject,
            _ => GeneratedField::Body,
        };
        Self::Leaf {
            field,
            word: WORDS[random.next_usize(WORDS.len())],
        }
    }

    fn render(&self) -> String {
        match self {
            Self::Leaf { field, word } => match field {
                GeneratedField::Anywhere => (*word).into(),
                GeneratedField::From => format!("from:{word}"),
                GeneratedField::Subject => format!("subject:{word}"),
                GeneratedField::Body => format!("body:{word}"),
            },
            Self::And(left, right) => format!("({} AND {})", left.render(), right.render()),
            Self::Or(left, right) => format!("({} OR {})", left.render(), right.render()),
            Self::Not(node) => format!("NOT ({})", node.render()),
        }
    }

    fn evaluate(&self, message: &GeneratedMessage) -> bool {
        match self {
            Self::Leaf { field, word } => match field {
                GeneratedField::Anywhere => [
                    message.from.as_str(),
                    message.subject.as_str(),
                    message.body.as_str(),
                ]
                .into_iter()
                .any(|value| contains_word(value, word)),
                GeneratedField::From => contains_word(&message.from, word),
                GeneratedField::Subject => contains_word(&message.subject, word),
                GeneratedField::Body => contains_word(&message.body, word),
            },
            Self::And(left, right) => left.evaluate(message) && right.evaluate(message),
            Self::Or(left, right) => left.evaluate(message) || right.evaluate(message),
            Self::Not(node) => !node.evaluate(message),
        }
    }
}

#[derive(Debug)]
struct GeneratedMessage {
    from: String,
    subject: String,
    body: String,
}

impl GeneratedMessage {
    fn new(random: &mut SearchXorShift32) -> Self {
        Self {
            from: generated_words(random),
            subject: generated_words(random),
            body: generated_words(random),
        }
    }

    fn as_searchable(&self) -> SearchableMessage<'_> {
        SearchableMessage {
            from: &self.from,
            to: "",
            cc: "",
            bcc: "",
            subject: &self.subject,
            body: &self.body,
            mailboxes: &[],
            mailbox: "INBOX",
            received_on: today(),
            attachments: &[],
            is_read: false,
            is_flagged: false,
            is_replied: false,
            is_draft: false,
        }
    }
}

fn render_parsed_node(node: &SearchNode) -> String {
    match node {
        SearchNode::Term(SearchTerm::Text(text)) => text.value.clone(),
        SearchNode::Term(SearchTerm::Field { field, value }) => {
            let field = match field {
                SearchField::From => "from",
                SearchField::Subject => "subject",
                SearchField::Body => "body",
                _ => panic!("generator only produces from, subject, and body fields"),
            };
            format!("{field}:{}", value.value)
        }
        SearchNode::And(nodes) => format!(
            "({})",
            nodes
                .iter()
                .map(render_parsed_node)
                .collect::<Vec<_>>()
                .join(" AND ")
        ),
        SearchNode::Or(nodes) => format!(
            "({})",
            nodes
                .iter()
                .map(render_parsed_node)
                .collect::<Vec<_>>()
                .join(" OR ")
        ),
        SearchNode::Not(node) => format!("NOT ({})", render_parsed_node(node)),
        SearchNode::MatchAll => panic!("generator never emits MatchAll"),
        SearchNode::Term(_) => panic!("generator only emits text and field terms"),
    }
}

fn contains_word(value: &str, word: &str) -> bool {
    value.split_whitespace().any(|candidate| candidate == word)
}

fn generated_words(random: &mut SearchXorShift32) -> String {
    const WORDS: &[&str] = &["alpha", "bravo", "charlie", "delta", "echo"];
    (0..=random.next_usize(4))
        .map(|_| WORDS[random.next_usize(WORDS.len())])
        .collect::<Vec<_>>()
        .join(" ")
}

struct SearchXorShift32(u32);

impl SearchXorShift32 {
    fn next(&mut self) -> u32 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 17;
        self.0 ^= self.0 << 5;
        self.0
    }

    fn next_usize(&mut self, upper_bound: usize) -> usize {
        self.next() as usize % upper_bound
    }
}

#[test]
fn sql_and_imap_compilers_reject_controls_nested_inside_boolean_ast() {
    for injected in ["safe\rALL", "safe\nOR ALL", "safe\0UID SEARCH ALL"] {
        let expression = SearchExpression {
            root: SearchNode::Not(Box::new(SearchNode::Or(vec![
                SearchNode::Term(SearchTerm::Field {
                    field: SearchField::Subject,
                    value: SearchText {
                        value: "ordinary".into(),
                        quoted: false,
                        prefix: false,
                    },
                }),
                SearchNode::Term(SearchTerm::Field {
                    field: SearchField::From,
                    value: SearchText {
                        value: injected.into(),
                        quoted: false,
                        prefix: false,
                    },
                }),
            ]))),
        };
        assert_eq!(
            compile_sql_candidate(&expression, today())
                .unwrap_err()
                .kind,
            SqlSearchCompileErrorKind::ControlCharacter
        );
        assert_eq!(
            compile_generic_imap(&expression, today()).unwrap_err().kind,
            ImapCompileErrorKind::ControlCharacter
        );
    }
}

#[test]
fn hostile_printable_literals_stay_bound_or_escaped() {
    let payloads = ["x' OR 1=1 --", "x\\y\" z", "100%_complete", "x) (ALL) ("];
    for payload in payloads {
        let expression = SearchExpression {
            root: SearchNode::Term(SearchTerm::Field {
                field: SearchField::Subject,
                value: SearchText {
                    value: payload.into(),
                    quoted: false,
                    prefix: false,
                },
            }),
        };
        let sql = compile_sql_candidate(&expression, today()).unwrap();
        assert!(!sql.predicate.contains(payload));
        if sql.binds.is_empty() {
            assert_eq!(sql.predicate, "1 = 1");
            assert!(sql.requires_post_filter);
        } else {
            assert!(sql
                .binds
                .iter()
                .all(|bind| matches!(bind, SqlSearchBind::Text(_))));
        }

        let imap = compile_generic_imap(&expression, today()).unwrap();
        assert!(!imap.criteria.contains('\r'));
        assert!(!imap.criteria.contains('\n'));
        assert!(imap.criteria.starts_with("SUBJECT \"") || imap.criteria == "ALL");
    }
}

#[test]
fn continuation_cannot_resume_after_scope_account_or_mode_changes() {
    let registry = SearchSessionRegistry::default();
    let original = request("subject:invoice");
    let session = registry.begin(&original);
    let token = SearchContinuationV2::new(&session, &original, 50).encode();

    let mut variants = Vec::new();
    let mut changed = original.clone();
    changed.account_ids.reverse();
    variants.push(changed);
    let mut changed = original.clone();
    changed.scope.include_spam_trash = true;
    variants.push(changed);
    let mut changed = original.clone();
    changed.execution_mode = SearchExecutionMode::Local;
    variants.push(changed);

    for mut changed in variants {
        changed.continuation = Some(token.clone());
        assert_eq!(
            changed.decode_continuation(),
            Err(SearchContinuationError::QueryMismatch)
        );
        assert!(registry.current_for(session.session_id, &changed).is_none());
    }
}

#[test]
fn superseded_session_tokens_cannot_reactivate_stale_work() {
    let registry = SearchSessionRegistry::default();
    let first_request = request("from:alice");
    let first = registry.begin(&first_request);
    let first_token = SearchContinuationV2::new(&first, &first_request, 50).encode();
    let second = registry.begin(&request("from:bob"));

    let mut resumed = first_request;
    resumed.continuation = Some(first_token);
    let decoded = resumed.decode_continuation().unwrap().unwrap();
    assert!(first.is_cancelled());
    assert!(!registry.is_current(&first));
    assert!(registry.current_for(decoded.session_id, &resumed).is_none());
    assert!(registry.is_current(&second));
}

#[test]
fn provider_progress_is_account_isolated_and_rejects_stale_publication() {
    let registry = SearchSessionRegistry::default();
    let original = request("subject:invoice");
    let first = registry.begin(&original);
    let account_a = original.account_ids[0];
    let account_b = original.account_ids[1];
    let mut cursor = ProviderSearchCursor::default();
    cursor.mailbox_last_uid.insert("INBOX".into(), 40);
    assert!(registry.set_provider_progress(&first, account_a, cursor.clone(), false));
    assert_eq!(
        registry.provider_progress(&first, account_a),
        Some((cursor, false))
    );
    assert_eq!(
        registry.provider_progress(&first, account_b),
        Some((ProviderSearchCursor::default(), false))
    );

    let second = registry.begin(&request("subject:receipt"));
    assert!(!registry.set_provider_progress(
        &first,
        account_a,
        ProviderSearchCursor::default(),
        true,
    ));
    assert!(registry.provider_progress(&first, account_a).is_none());
    assert_eq!(
        registry.provider_progress(&second, account_a),
        Some((ProviderSearchCursor::default(), false))
    );
}
