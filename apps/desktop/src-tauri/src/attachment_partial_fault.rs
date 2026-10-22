//! One-shot bulk-save fault used only by the owned fictional native fixture.
//!
//! The marker is deliberately a file inside the sentinel-marked fixture
//! profile. Production builds do not enable the containing feature or compile
//! its call site.

use super::*;
#[cfg(test)]
use std::io::Write;
use std::{fs, io::ErrorKind};

const ROOT_SENTINEL: &str = ".dakia-native-mail-safety-root";
const PROFILE_SENTINEL: &str = ".dakia-native-mail-safety-profile";
const PARTIAL_SAVE_MARKER: &str = ".dakia-native-mail-safety-partial-save-once";
const PARTIAL_SAVE_MARKER_CONTENTS: &str = "dakia-native-mail-safety-partial-save-v1\n";

struct OwnedFixtureProfile {
    profile: PathBuf,
}

fn owned_fixture_profile(root: &Path, profile: &Path) -> anyhow::Result<OwnedFixtureProfile> {
    if !root.is_absolute() || !profile.is_absolute() {
        anyhow::bail!("native partial-save fixture paths must be absolute");
    }
    let root = root.canonicalize()?;
    if fs::read_to_string(root.join(ROOT_SENTINEL)).ok().as_deref()
        != Some(NATIVE_MAIL_SAFETY_FIXTURE_MARKER)
    {
        anyhow::bail!("native partial-save fixture root lacks its exact ownership sentinel");
    }
    let profile = profile.canonicalize()?;
    if profile == root || !profile.starts_with(&root) {
        anyhow::bail!("native partial-save fixture profile must be inside its root");
    }
    if fs::read_to_string(profile.join(PROFILE_SENTINEL))
        .ok()
        .as_deref()
        != Some(NATIVE_MAIL_SAFETY_FIXTURE_MARKER)
    {
        anyhow::bail!("native partial-save fixture profile lacks its exact ownership sentinel");
    }
    Ok(OwnedFixtureProfile { profile })
}

fn owned_fixture_profile_from_environment() -> anyhow::Result<Option<OwnedFixtureProfile>> {
    match std::env::var("DAKIA_NATIVE_MAIL_SAFETY_FIXTURE") {
        Ok(value) if value == "1" => {}
        Ok(_) => anyhow::bail!("native mail-safety fixture marker must equal 1"),
        Err(std::env::VarError::NotPresent) => return Ok(None),
        Err(error) => return Err(error.into()),
    }
    let required = |name: &str| {
        std::env::var(name).map_err(|_| anyhow::anyhow!("{name} is required for partial save"))
    };
    Ok(Some(owned_fixture_profile(
        Path::new(&required("DAKIA_NATIVE_MAIL_SAFETY_ROOT")?),
        Path::new(&required("DAKIA_NATIVE_MAIL_SAFETY_DATA_DIR")?),
    )?))
}

fn marker_path(fixture: &OwnedFixtureProfile) -> PathBuf {
    fixture.profile.join(PARTIAL_SAVE_MARKER)
}

#[cfg(test)]
fn arm_owned_fixture_marker(fixture: &OwnedFixtureProfile) -> anyhow::Result<()> {
    let marker = marker_path(fixture);
    fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&marker)?
        .write_all(PARTIAL_SAVE_MARKER_CONTENTS.as_bytes())?;
    Ok(())
}

fn consume_owned_fixture_marker(fixture: &OwnedFixtureProfile) -> anyhow::Result<bool> {
    let marker = marker_path(fixture);
    let metadata = match fs::symlink_metadata(&marker) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error.into()),
    };
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        anyhow::bail!("native partial-save fixture marker must be a regular file");
    }
    if fs::read_to_string(&marker)? != PARTIAL_SAVE_MARKER_CONTENTS {
        anyhow::bail!("native partial-save fixture marker contents are invalid");
    }
    fs::remove_file(marker)?;
    Ok(true)
}

/// Arms the next fixture-only batch operation. This is called by the real
/// provider-to-Reader artifact test after it has proved the normal success
/// path, so the marker cannot obscure that baseline.
#[cfg(test)]
pub(super) fn arm_partial_after_first_save() -> anyhow::Result<()> {
    let fixture = owned_fixture_profile_from_environment()?
        .ok_or_else(|| anyhow::anyhow!("native partial-save fixture is not enabled"))?;
    arm_owned_fixture_marker(&fixture)
}

/// After the first actual publish, consume the owned marker. The caller feeds
/// the returned one-shot failure into its existing batch confirmation path,
/// which creates Dakia's normal structured partial-save result before a
/// second file is written. No mailbox state changes, so an IMAP watcher cannot
/// race this fixture-only fault.
pub(super) fn inject_partial_after_successful_bulk_save(
    saved_count: usize,
) -> anyhow::Result<bool> {
    if saved_count != 1 {
        return Ok(false);
    }
    let Some(fixture) = owned_fixture_profile_from_environment()? else {
        return Ok(false);
    };
    consume_owned_fixture_marker(&fixture)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn owned_profile() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let temp = tempdir().expect("temporary fixture root");
        let root = temp.path().join("root");
        let profile = root.join("profile");
        fs::create_dir_all(&profile).expect("fixture profile");
        fs::write(root.join(ROOT_SENTINEL), NATIVE_MAIL_SAFETY_FIXTURE_MARKER)
            .expect("root sentinel");
        fs::write(
            profile.join(PROFILE_SENTINEL),
            NATIVE_MAIL_SAFETY_FIXTURE_MARKER,
        )
        .expect("profile sentinel");
        (temp, root, profile)
    }

    #[test]
    fn partial_marker_requires_owned_root_and_profile_and_is_one_shot() {
        let (_temp, root, profile) = owned_profile();
        let fixture = owned_fixture_profile(&root, &profile).expect("owned fixture profile");
        arm_owned_fixture_marker(&fixture).expect("arm fixture-local marker");
        assert!(marker_path(&fixture).is_file());
        assert!(consume_owned_fixture_marker(&fixture).expect("consume marker"));
        assert!(
            !consume_owned_fixture_marker(&fixture).expect("marker cannot fire twice"),
            "the marker must be consumed before a later batch can observe it"
        );

        fs::remove_file(profile.join(PROFILE_SENTINEL)).expect("remove test sentinel");
        assert!(
            owned_fixture_profile(&root, &profile).is_err(),
            "a profile without the exact sentinel cannot arm the fault"
        );
    }
}
