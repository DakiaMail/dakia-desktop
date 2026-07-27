use reqwest::Client;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::future::Future;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::time::Duration;
use tauri::ipc::Channel;
use tokio::io::AsyncWriteExt;

const MANIFEST_JSON: &str = include_str!("translation_models.json");
const MODEL_DIRECTORY: &str = "translation-models";
const PRODUCTION_DOWNLOAD_POLICY: DownloadPolicy = DownloadPolicy {
    scheme: "https",
    host: "firefox-settings-attachments.cdn.mozilla.net",
};
const PRODUCTION_DOWNLOAD_TIMEOUTS: DownloadTimeouts = DownloadTimeouts {
    connect: Duration::from_secs(15),
    request: Duration::from_secs(30 * 60),
    idle: Duration::from_secs(30),
};
const CANCELLATION_POLL_INTERVAL: Duration = Duration::from_millis(100);

#[derive(Clone, Copy)]
struct DownloadPolicy {
    scheme: &'static str,
    host: &'static str,
}

#[derive(Clone, Copy)]
struct DownloadTimeouts {
    connect: Duration,
    request: Duration,
    idle: Duration,
}

async fn await_or_cancel<F, T>(future: F, cancelled: &AtomicBool) -> Result<T, String>
where
    F: Future<Output = T>,
{
    tokio::pin!(future);
    let mut cancellation_poll = tokio::time::interval(CANCELLATION_POLL_INTERVAL);
    cancellation_poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            biased;
            _ = cancellation_poll.tick() => {
                if cancelled.load(Ordering::Relaxed) {
                    return Err("Language pack download cancelled.".to_owned());
                }
            }
            result = &mut future => return Ok(result),
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
#[cfg_attr(not(test), allow(dead_code))]
struct Manifest {
    #[serde(rename = "registryUrl")]
    registry_url: String,
    #[serde(rename = "registryLastModified")]
    registry_last_modified: u64,
    models: Vec<ModelManifest>,
    #[serde(rename = "excludedModels")]
    excluded_models: Vec<ModelExclusion>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(not(test), allow(dead_code))]
struct ModelManifest {
    source: String,
    source_name: String,
    target: String,
    registry_source: String,
    version: String,
    files: ModelFiles,
    #[serde(default)]
    config: serde_json::Value,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(not(test), allow(dead_code))]
struct ModelExclusion {
    source: String,
    source_name: String,
    reason: String,
}

#[derive(Clone, Debug, Deserialize)]
struct ModelFiles {
    model: Artifact,
    shortlist: Artifact,
    vocabs: Vec<Artifact>,
}

#[derive(Clone, Debug, Deserialize)]
struct Artifact(String, u64, String);

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TranslationModelStatus {
    pub source: String,
    pub source_name: String,
    pub target: String,
    pub download_bytes: u64,
    pub installed: bool,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TranslationDownloadProgress {
    pub source: String,
    pub downloaded_bytes: u64,
    pub total_bytes: u64,
    pub file_index: usize,
    pub file_count: usize,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TranslationModelFiles {
    pub source: String,
    pub target: String,
    pub model_path: PathBuf,
    pub shortlist_path: PathBuf,
    pub vocab_paths: Vec<PathBuf>,
    pub config: serde_json::Value,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TranslationLanguageDetection {
    pub language: String,
    pub language_name: String,
    pub reliable: bool,
}

fn language_code(language: whatlang::Lang) -> &'static str {
    match language {
        whatlang::Lang::Afr => "af",
        whatlang::Lang::Aka => "ak",
        whatlang::Lang::Amh => "am",
        whatlang::Lang::Ara => "ar",
        whatlang::Lang::Aze => "az",
        whatlang::Lang::Bel => "be",
        whatlang::Lang::Ben => "bn",
        whatlang::Lang::Bul => "bg",
        whatlang::Lang::Cat => "ca",
        whatlang::Lang::Ces => "cs",
        whatlang::Lang::Cmn => "zh",
        whatlang::Lang::Cym => "cy",
        whatlang::Lang::Dan => "da",
        whatlang::Lang::Deu => "de",
        whatlang::Lang::Ell => "el",
        whatlang::Lang::Eng => "en",
        whatlang::Lang::Epo => "eo",
        whatlang::Lang::Est => "et",
        whatlang::Lang::Fin => "fi",
        whatlang::Lang::Fra => "fr",
        whatlang::Lang::Guj => "gu",
        whatlang::Lang::Heb => "he",
        whatlang::Lang::Hin => "hi",
        whatlang::Lang::Hrv => "hr",
        whatlang::Lang::Hun => "hu",
        whatlang::Lang::Hye => "hy",
        whatlang::Lang::Ind => "id",
        whatlang::Lang::Ita => "it",
        whatlang::Lang::Jav => "jv",
        whatlang::Lang::Jpn => "ja",
        whatlang::Lang::Kan => "kn",
        whatlang::Lang::Kat => "ka",
        whatlang::Lang::Khm => "km",
        whatlang::Lang::Kor => "ko",
        whatlang::Lang::Lat => "la",
        whatlang::Lang::Lav => "lv",
        whatlang::Lang::Lit => "lt",
        whatlang::Lang::Mal => "ml",
        whatlang::Lang::Mar => "mr",
        whatlang::Lang::Mkd => "mk",
        whatlang::Lang::Mya => "my",
        whatlang::Lang::Nep => "ne",
        whatlang::Lang::Nld => "nl",
        whatlang::Lang::Nob => "nb",
        whatlang::Lang::Ori => "or",
        whatlang::Lang::Pan => "pa",
        whatlang::Lang::Pes => "fa",
        whatlang::Lang::Pol => "pl",
        whatlang::Lang::Por => "pt",
        whatlang::Lang::Ron => "ro",
        whatlang::Lang::Rus => "ru",
        whatlang::Lang::Sin => "si",
        whatlang::Lang::Slk => "sk",
        whatlang::Lang::Slv => "sl",
        whatlang::Lang::Sna => "sn",
        whatlang::Lang::Spa => "es",
        whatlang::Lang::Srp => "sr",
        whatlang::Lang::Swe => "sv",
        whatlang::Lang::Tam => "ta",
        whatlang::Lang::Tel => "te",
        whatlang::Lang::Tgl => "tl",
        whatlang::Lang::Tha => "th",
        whatlang::Lang::Tuk => "tk",
        whatlang::Lang::Tur => "tr",
        whatlang::Lang::Ukr => "uk",
        whatlang::Lang::Urd => "ur",
        whatlang::Lang::Uzb => "uz",
        whatlang::Lang::Vie => "vi",
        whatlang::Lang::Yid => "yi",
        whatlang::Lang::Zul => "zu",
    }
}

fn language_name(language: whatlang::Lang) -> &'static str {
    match language {
        whatlang::Lang::Cmn => "Chinese",
        _ => language.eng_name(),
    }
}

pub fn detect_language(text: &str) -> TranslationLanguageDetection {
    let normalized = text
        .lines()
        .filter(|line| !line.trim_start().starts_with('>'))
        .flat_map(str::split_whitespace)
        .collect::<Vec<_>>()
        .join(" ");
    let sample = normalized.chars().take(10_000).collect::<String>();
    let detector = whatlang::Detector::new();
    let Some(info) = detector.detect(&sample) else {
        return TranslationLanguageDetection {
            language: "und".to_owned(),
            language_name: "Unknown language".to_owned(),
            reliable: false,
        };
    };
    TranslationLanguageDetection {
        language: language_code(info.lang()).to_owned(),
        language_name: language_name(info.lang()).to_owned(),
        reliable: info.is_reliable() && info.confidence() >= 0.7,
    }
}

fn manifest() -> Result<Manifest, String> {
    serde_json::from_str(MANIFEST_JSON)
        .map_err(|error| format!("Translation model manifest is invalid: {error}"))
}

fn model_for(source: &str) -> Result<ModelManifest, String> {
    manifest()?
        .models
        .into_iter()
        .find(|model| model.source == source && model.target == "en")
        .ok_or_else(|| format!("No offline English translation model is available for {source}."))
}

fn model_dir(data_dir: &Path, source: &str) -> PathBuf {
    data_dir.join(MODEL_DIRECTORY).join(format!("{source}-en"))
}

fn artifact_paths(data_dir: &Path, model: &ModelManifest) -> TranslationModelFiles {
    let directory = model_dir(data_dir, &model.source);
    TranslationModelFiles {
        source: model.source.clone(),
        target: model.target.clone(),
        model_path: directory.join("model.bin"),
        shortlist_path: directory.join("shortlist.bin"),
        vocab_paths: model
            .files
            .vocabs
            .iter()
            .enumerate()
            .map(|(index, _)| directory.join(format!("vocab-{index}.spm")))
            .collect(),
        config: model.config.clone(),
    }
}

fn expected_files(data_dir: &Path, model: &ModelManifest) -> Vec<(Artifact, PathBuf)> {
    let paths = artifact_paths(data_dir, model);
    let mut files = vec![
        (model.files.model.clone(), paths.model_path),
        (model.files.shortlist.clone(), paths.shortlist_path),
    ];
    files.extend(model.files.vocabs.iter().cloned().zip(paths.vocab_paths));
    files
}

fn present(data_dir: &Path, model: &ModelManifest) -> bool {
    expected_files(data_dir, model)
        .iter()
        .all(|(artifact, path)| {
            path.is_file() && path.metadata().is_ok_and(|meta| meta.len() == artifact.1)
        })
}

fn verified(data_dir: &Path, model: &ModelManifest) -> Result<bool, String> {
    if !present(data_dir, model) {
        return Ok(false);
    }
    for (artifact, path) in expected_files(data_dir, model) {
        let mut file = std::fs::File::open(&path)
            .map_err(|error| format!("Could not verify the language pack: {error}"))?;
        let mut hash = Sha256::new();
        let mut buffer = [0u8; 64 * 1024];
        loop {
            let read = file
                .read(&mut buffer)
                .map_err(|error| format!("Could not verify the language pack: {error}"))?;
            if read == 0 {
                break;
            }
            hash.update(&buffer[..read]);
        }
        if format!("{:x}", hash.finalize()) != artifact.2 {
            return Ok(false);
        }
    }
    Ok(true)
}

pub fn statuses(data_dir: &Path) -> Result<Vec<TranslationModelStatus>, String> {
    Ok(manifest()?
        .models
        .iter()
        .map(|model| TranslationModelStatus {
            source: model.source.clone(),
            source_name: model.source_name.clone(),
            target: model.target.clone(),
            download_bytes: model.files.model.1
                + model.files.shortlist.1
                + model
                    .files
                    .vocabs
                    .iter()
                    .map(|artifact| artifact.1)
                    .sum::<u64>(),
            installed: verified(data_dir, model).unwrap_or(false),
        })
        .collect())
}

pub fn files(data_dir: &Path, source: &str) -> Result<TranslationModelFiles, String> {
    let model = model_for(source)?;
    if !verified(data_dir, &model)? {
        return Err(format!(
            "The offline {} to English language pack is missing or failed its integrity check.",
            source
        ));
    }
    Ok(artifact_paths(data_dir, &model))
}

pub async fn install(
    data_dir: &Path,
    source: &str,
    on_progress: Channel<TranslationDownloadProgress>,
    cancelled: Arc<AtomicBool>,
) -> Result<TranslationModelFiles, String> {
    let model = model_for(source)?;
    install_model(
        data_dir,
        &model,
        |progress| {
            let _ = on_progress.send(progress);
        },
        cancelled,
        PRODUCTION_DOWNLOAD_POLICY,
        PRODUCTION_DOWNLOAD_TIMEOUTS,
    )
    .await
}

async fn install_model(
    data_dir: &Path,
    model: &ModelManifest,
    mut on_progress: impl FnMut(TranslationDownloadProgress),
    cancelled: Arc<AtomicBool>,
    download_policy: DownloadPolicy,
    download_timeouts: DownloadTimeouts,
) -> Result<TranslationModelFiles, String> {
    let source = &model.source;
    if verified(data_dir, model)? {
        return Ok(artifact_paths(data_dir, model));
    }

    let root = data_dir.join(MODEL_DIRECTORY);
    tokio::fs::create_dir_all(&root)
        .await
        .map_err(|error| format!("Could not create the translation model directory: {error}"))?;
    let final_dir = model_dir(data_dir, source);
    let temp_dir = root.join(format!(".{source}-en.download"));
    if temp_dir.exists() {
        tokio::fs::remove_dir_all(&temp_dir)
            .await
            .map_err(|error| format!("Could not clear an incomplete model download: {error}"))?;
    }
    tokio::fs::create_dir(&temp_dir)
        .await
        .map_err(|error| format!("Could not prepare the model download: {error}"))?;

    let client = Client::builder()
        .redirect(reqwest::redirect::Policy::limited(3))
        .connect_timeout(download_timeouts.connect)
        .timeout(download_timeouts.request)
        .build()
        .map_err(|error| format!("Could not initialize the model downloader: {error}"))?;
    let total_bytes = model.files.model.1
        + model.files.shortlist.1
        + model
            .files
            .vocabs
            .iter()
            .map(|artifact| artifact.1)
            .sum::<u64>();
    let final_paths = artifact_paths(data_dir, model);
    let names = std::iter::once("model.bin".to_owned())
        .chain(std::iter::once("shortlist.bin".to_owned()))
        .chain(
            model
                .files
                .vocabs
                .iter()
                .enumerate()
                .map(|(index, _)| format!("vocab-{index}.spm")),
        )
        .collect::<Vec<_>>();
    let artifacts = std::iter::once(&model.files.model)
        .chain(std::iter::once(&model.files.shortlist))
        .chain(model.files.vocabs.iter())
        .collect::<Vec<_>>();
    let mut downloaded_bytes = 0u64;

    let result = async {
        for (index, (artifact, name)) in artifacts.iter().zip(names.iter()).enumerate() {
            if cancelled.load(Ordering::Relaxed) {
                return Err("Language pack download cancelled.".to_owned());
            }
            let url = reqwest::Url::parse(&artifact.0)
                .map_err(|error| format!("Invalid model URL: {error}"))?;
            if url.scheme() != download_policy.scheme
                || url.host_str() != Some(download_policy.host)
            {
                return Err("The translation model source is not trusted.".to_owned());
            }
            let mut response = await_or_cancel(client.get(url).send(), cancelled.as_ref())
                .await?
                .map_err(|error| format!("Could not download the language pack: {error}"))?
                .error_for_status()
                .map_err(|error| format!("The language pack download failed: {error}"))?;
            let path = temp_dir.join(name);
            let mut output = tokio::fs::File::create(&path)
                .await
                .map_err(|error| format!("Could not save the language pack: {error}"))?;
            let mut hash = Sha256::new();
            let mut file_bytes = 0u64;
            while let Some(chunk) = await_or_cancel(
                tokio::time::timeout(download_timeouts.idle, response.chunk()),
                cancelled.as_ref(),
            )
            .await?
            .map_err(|_| "The language pack download stalled without receiving data.".to_owned())?
            .map_err(|error| format!("The language pack download was interrupted: {error}"))?
            {
                if cancelled.load(Ordering::Relaxed) {
                    return Err("Language pack download cancelled.".to_owned());
                }
                output
                    .write_all(&chunk)
                    .await
                    .map_err(|error| format!("Could not save the language pack: {error}"))?;
                hash.update(&chunk);
                file_bytes += chunk.len() as u64;
                downloaded_bytes += chunk.len() as u64;
                on_progress(TranslationDownloadProgress {
                    source: source.to_owned(),
                    downloaded_bytes,
                    total_bytes,
                    file_index: index + 1,
                    file_count: artifacts.len(),
                });
            }
            output
                .flush()
                .await
                .map_err(|error| format!("Could not finish saving the language pack: {error}"))?;
            if file_bytes != artifact.1 || format!("{:x}", hash.finalize()) != artifact.2 {
                return Err("The language pack failed its integrity check.".to_owned());
            }
        }

        if final_dir.exists() {
            tokio::fs::remove_dir_all(&final_dir)
                .await
                .map_err(|error| format!("Could not replace the language pack: {error}"))?;
        }
        tokio::fs::rename(&temp_dir, &final_dir)
            .await
            .map_err(|error| format!("Could not install the language pack: {error}"))?;
        Ok(final_paths)
    }
    .await;

    if result.is_err() && temp_dir.exists() {
        let _ = tokio::fs::remove_dir_all(&temp_dir).await;
    }
    result
}

pub async fn remove(data_dir: &Path, source: &str) -> Result<(), String> {
    model_for(source)?;
    let directory = model_dir(data_dir, source);
    if directory.exists() {
        tokio::fs::remove_dir_all(directory)
            .await
            .map_err(|error| format!("Could not remove the language pack: {error}"))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::{write::GzEncoder, Compression};
    use std::collections::HashMap;
    use std::io::Write as _;
    use std::net::TcpListener;
    use std::thread;

    fn hash(bytes: &[u8]) -> String {
        format!("{:x}", Sha256::digest(bytes))
    }

    fn artifact(url: String, bytes: &[u8]) -> Artifact {
        Artifact(url, bytes.len() as u64, hash(bytes))
    }

    fn test_model(base_url: &str, model: &[u8], shortlist: &[u8], vocab: &[u8]) -> ModelManifest {
        ModelManifest {
            source: "et".to_owned(),
            source_name: "Estonian".to_owned(),
            target: "en".to_owned(),
            registry_source: "et".to_owned(),
            version: "test".to_owned(),
            files: ModelFiles {
                model: artifact(format!("{base_url}/model"), model),
                shortlist: artifact(format!("{base_url}/shortlist"), shortlist),
                vocabs: vec![artifact(format!("{base_url}/vocab"), vocab)],
            },
            config: serde_json::json!({ "beam-size": "1" }),
        }
    }

    type TestResponse = (Vec<u8>, Option<&'static str>);

    fn raw(bytes: Vec<u8>) -> TestResponse {
        (bytes, None)
    }

    fn gzipped(bytes: &[u8]) -> TestResponse {
        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(bytes).unwrap();
        (encoder.finish().unwrap(), Some("gzip"))
    }

    fn serve(responses: HashMap<&'static str, TestResponse>) -> (String, thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let expected_requests = responses.len();
        let handle = thread::spawn(move || {
            for stream in listener.incoming().take(expected_requests) {
                let mut stream = stream.unwrap();
                let mut request = [0u8; 2048];
                let read = stream.read(&mut request).unwrap();
                let request = String::from_utf8_lossy(&request[..read]);
                let path = request
                    .lines()
                    .next()
                    .and_then(|line| line.split_whitespace().nth(1))
                    .unwrap();
                let (body, content_encoding) = responses.get(path).unwrap();
                let encoding_header = content_encoding
                    .map(|encoding| format!("Content-Encoding: {encoding}\r\n"))
                    .unwrap_or_default();
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n{}Connection: close\r\n\r\n",
                    body.len(),
                    encoding_header
                )
                .unwrap();
                stream.write_all(body).unwrap();
            }
        });
        (format!("http://{address}"), handle)
    }

    fn serve_stalled() -> (String, thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let handle = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0u8; 2048];
            let _ = stream.read(&mut request).unwrap();
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 10\r\nConnection: close\r\n\r\nx")
                .unwrap();
            thread::sleep(Duration::from_millis(200));
        });
        (format!("http://{address}"), handle)
    }

    fn serve_stalled_headers() -> (String, thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let handle = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0u8; 2048];
            let _ = stream.read(&mut request).unwrap();
            thread::sleep(Duration::from_millis(250));
        });
        (format!("http://{address}"), handle)
    }

    fn local_policy() -> DownloadPolicy {
        DownloadPolicy {
            scheme: "http",
            host: "127.0.0.1",
        }
    }

    fn test_timeouts() -> DownloadTimeouts {
        DownloadTimeouts {
            connect: Duration::from_secs(2),
            request: Duration::from_secs(5),
            idle: Duration::from_secs(2),
        }
    }

    #[test]
    fn pinned_manifest_has_unique_direct_to_english_models() {
        let manifest = manifest().unwrap();
        assert_eq!(
            manifest.registry_url,
            "https://firefox.settings.services.mozilla.com/v1/buckets/main/collections/translations-models/records"
        );
        assert!(manifest.registry_last_modified > 0);
        let mut sources = manifest
            .models
            .iter()
            .map(|model| model.source.as_str())
            .collect::<Vec<_>>();
        let count = sources.len();
        sources.sort_unstable();
        sources.dedup();
        assert_eq!(sources.len(), count);
        assert!(manifest.models.iter().all(|model| model.target == "en"));
        for required in ["ar", "et", "ja", "zh"] {
            assert!(sources.contains(&required));
        }
        assert_eq!(sources.len(), 43);
        assert!(manifest
            .excluded_models
            .iter()
            .all(|model| !model.source.is_empty()
                && !model.source_name.is_empty()
                && !model.reason.is_empty()));
        for model in manifest.models {
            assert!(!model.source_name.is_empty());
            assert!(!model.registry_source.is_empty());
            assert!(!model.version.is_empty());
            for (artifact, _) in expected_files(Path::new("/tmp"), &model) {
                let url = reqwest::Url::parse(&artifact.0).unwrap();
                assert_eq!(url.scheme(), PRODUCTION_DOWNLOAD_POLICY.scheme);
                assert_eq!(url.host_str(), Some(PRODUCTION_DOWNLOAD_POLICY.host));
                assert!(artifact.1 > 0);
                assert_eq!(artifact.2.len(), 64);
                assert!(artifact
                    .2
                    .chars()
                    .all(|character| character.is_ascii_hexdigit()));
            }
        }
    }

    #[test]
    fn rejects_unknown_language() {
        assert!(model_for("xx").unwrap_err().contains("No offline English"));
    }

    #[test]
    fn maps_every_whatlang_language_to_a_stable_bcp_47_code() {
        let mut codes = whatlang::Lang::all()
            .iter()
            .copied()
            .map(language_code)
            .collect::<Vec<_>>();
        assert_eq!(codes.len(), 70);
        assert!(codes.iter().all(|code| code.len() == 2));
        codes.sort_unstable();
        codes.dedup();
        assert_eq!(codes.len(), 70);
    }

    #[test]
    fn detects_real_language_and_email_noise_fixtures() {
        for (fixture, expected_code, expected_name) in [
            (
                include_str!("../testdata/translation_detection/arabic.txt"),
                "ar",
                "Arabic",
            ),
            (
                include_str!("../testdata/translation_detection/chinese.txt"),
                "zh",
                "Chinese",
            ),
            (
                include_str!("../testdata/translation_detection/japanese.txt"),
                "ja",
                "Japanese",
            ),
            (
                include_str!("../testdata/translation_detection/mixed_language.txt"),
                "zh",
                "Chinese",
            ),
            (
                include_str!("../testdata/translation_detection/signature_heavy.txt"),
                "fr",
                "French",
            ),
            (
                include_str!("../testdata/translation_detection/quoted_thread.txt"),
                "es",
                "Spanish",
            ),
        ] {
            let detection = detect_language(fixture);
            assert_eq!(detection.language, expected_code, "{fixture}");
            assert_eq!(detection.language_name, expected_name);
            assert!(detection.reliable, "{fixture}");
        }
    }

    #[test]
    fn detects_supported_languages_with_two_letter_codes() {
        let estonian = detect_language(
            "See on piisavalt pikk eestikeelne tekst, mis kirjeldab turvalist võrguühenduseta tõlkimist otse kasutaja arvutis.",
        );
        assert_eq!(estonian.language, "et");
        assert_eq!(estonian.language_name, "Estonian");
        assert!(estonian.reliable);

        let english = detect_language(
            "This is a sufficiently long English passage about translating email privately on the user's own computer.",
        );
        assert_eq!(english.language, "en");
        assert_eq!(english.language_name, "English");
        assert!(english.reliable);
    }

    #[test]
    fn short_and_empty_inputs_are_not_treated_as_reliable() {
        for fixture in [
            include_str!("../testdata/translation_detection/short_body.txt"),
            "   ",
        ] {
            let detection = detect_language(fixture);
            assert!(!detection.reliable);
        }
        let empty = detect_language("   ");
        assert_eq!(empty.language, "und");
        assert_eq!(empty.language_name, "Unknown language");
    }

    #[test]
    fn real_detection_routes_to_models_or_an_explicit_refusal() {
        for (fixture, expected_source) in [
            (
                include_str!("../testdata/translation_detection/arabic.txt"),
                "ar",
            ),
            (
                include_str!("../testdata/translation_detection/chinese.txt"),
                "zh",
            ),
            (
                include_str!("../testdata/translation_detection/japanese.txt"),
                "ja",
            ),
        ] {
            let detection = detect_language(fixture);
            assert!(detection.reliable);
            let model = model_for(&detection.language).unwrap();
            assert_eq!(model.source, expected_source);
            assert_eq!(model.target, "en");
        }

        let unsupported = detect_language(
            "This Esperanto message is deliberately long enough to be detected reliably. Ĉiuj homoj estas denaske liberaj kaj egalaj laŭ digno kaj rajtoj.",
        );
        assert_eq!(unsupported.language, "eo");
        assert!(unsupported.reliable);
        assert!(model_for(&unsupported.language).is_err());
    }

    #[test]
    fn verifies_every_artifact_and_rejects_same_size_corruption() {
        let directory = tempfile::tempdir().unwrap();
        let model_bytes = b"model";
        let shortlist_bytes = b"short";
        let vocab_bytes = b"vocab";
        let model = test_model(
            "http://127.0.0.1",
            model_bytes,
            shortlist_bytes,
            vocab_bytes,
        );
        for ((_, path), bytes) in expected_files(directory.path(), &model).into_iter().zip([
            model_bytes.as_slice(),
            shortlist_bytes.as_slice(),
            vocab_bytes.as_slice(),
        ]) {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, bytes).unwrap();
        }
        assert!(verified(directory.path(), &model).unwrap());

        std::fs::write(
            artifact_paths(directory.path(), &model).model_path,
            b"modem",
        )
        .unwrap();
        assert!(present(directory.path(), &model));
        assert!(!verified(directory.path(), &model).unwrap());
    }

    #[tokio::test]
    async fn downloads_verifies_and_atomically_installs_a_model_pack() {
        let model_bytes = b"model-data".to_vec();
        let shortlist_bytes = b"short-list".to_vec();
        let vocab_bytes = b"vocabulary".to_vec();
        let (base_url, server) = serve(HashMap::from([
            ("/model", gzipped(&model_bytes)),
            ("/shortlist", raw(shortlist_bytes.clone())),
            ("/vocab", raw(vocab_bytes.clone())),
        ]));
        let model = test_model(&base_url, &model_bytes, &shortlist_bytes, &vocab_bytes);
        let directory = tempfile::tempdir().unwrap();
        for (artifact, path) in expected_files(directory.path(), &model) {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, vec![0; artifact.1 as usize]).unwrap();
        }
        assert!(present(directory.path(), &model));
        assert!(!verified(directory.path(), &model).unwrap());
        let mut progress = Vec::new();

        let files = install_model(
            directory.path(),
            &model,
            |update| progress.push(update),
            Arc::new(AtomicBool::new(false)),
            local_policy(),
            test_timeouts(),
        )
        .await
        .unwrap();
        server.join().unwrap();

        assert_eq!(std::fs::read(files.model_path).unwrap(), model_bytes);
        assert_eq!(
            std::fs::read(files.shortlist_path).unwrap(),
            shortlist_bytes
        );
        assert_eq!(std::fs::read(&files.vocab_paths[0]).unwrap(), vocab_bytes);
        assert!(verified(directory.path(), &model).unwrap());
        assert!(!directory
            .path()
            .join(MODEL_DIRECTORY)
            .join(".et-en.download")
            .exists());
        assert!(!progress.is_empty());
        assert_eq!(
            progress.last().unwrap().downloaded_bytes,
            model_bytes.len() as u64 + shortlist_bytes.len() as u64 + vocab_bytes.len() as u64
        );
        assert_eq!(progress.last().unwrap().file_count, 3);
    }

    #[tokio::test]
    async fn rejects_corrupt_download_and_cleans_partial_files() {
        let expected_model = b"model-data";
        let corrupt_model = b"model-date";
        let shortlist = b"short-list";
        let vocab = b"vocabulary";
        let (base_url, server) = serve(HashMap::from([("/model", raw(corrupt_model.to_vec()))]));
        let model = test_model(&base_url, expected_model, shortlist, vocab);
        let directory = tempfile::tempdir().unwrap();

        let error = install_model(
            directory.path(),
            &model,
            |_| {},
            Arc::new(AtomicBool::new(false)),
            local_policy(),
            test_timeouts(),
        )
        .await
        .unwrap_err();
        server.join().unwrap();

        assert!(error.contains("integrity check"));
        assert!(!model_dir(directory.path(), "et").exists());
        assert!(!directory
            .path()
            .join(MODEL_DIRECTORY)
            .join(".et-en.download")
            .exists());
    }

    #[tokio::test]
    async fn cancellation_stops_before_network_and_leaves_no_partial_pack() {
        let model = test_model("http://127.0.0.1:9", b"model", b"short", b"vocab");
        let directory = tempfile::tempdir().unwrap();
        let cancelled = Arc::new(AtomicBool::new(true));

        let error = install_model(
            directory.path(),
            &model,
            |_| panic!("cancelled downloads must not report progress"),
            cancelled,
            local_policy(),
            test_timeouts(),
        )
        .await
        .unwrap_err();

        assert_eq!(error, "Language pack download cancelled.");
        assert!(!model_dir(directory.path(), "et").exists());
    }

    #[tokio::test]
    async fn rejects_untrusted_model_source_before_requesting_it() {
        let model = test_model("http://example.com", b"model", b"short", b"vocab");
        let directory = tempfile::tempdir().unwrap();

        let error = install_model(
            directory.path(),
            &model,
            |_| {},
            Arc::new(AtomicBool::new(false)),
            local_policy(),
            test_timeouts(),
        )
        .await
        .unwrap_err();

        assert_eq!(error, "The translation model source is not trusted.");
    }

    #[tokio::test]
    async fn stalled_download_times_out_and_cleans_partial_files() {
        let (base_url, server) = serve_stalled();
        let model = test_model(&base_url, b"0123456789", b"short", b"vocab");
        let directory = tempfile::tempdir().unwrap();

        let error = install_model(
            directory.path(),
            &model,
            |_| {},
            Arc::new(AtomicBool::new(false)),
            local_policy(),
            DownloadTimeouts {
                connect: Duration::from_secs(1),
                request: Duration::from_secs(1),
                idle: Duration::from_millis(50),
            },
        )
        .await
        .unwrap_err();
        server.join().unwrap();

        assert!(error.contains("stalled"), "{error}");
        assert!(!model_dir(directory.path(), "et").exists());
        assert!(!directory
            .path()
            .join(MODEL_DIRECTORY)
            .join(".et-en.download")
            .exists());
    }

    #[tokio::test]
    async fn cancellation_interrupts_stalled_response_headers_and_cleans_partial_files() {
        let (base_url, server) = serve_stalled_headers();
        let model = test_model(&base_url, b"model", b"short", b"vocab");
        let directory = tempfile::tempdir().unwrap();
        let cancelled = Arc::new(AtomicBool::new(false));
        let cancel_after_request = Arc::clone(&cancelled);
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(25)).await;
            cancel_after_request.store(true, Ordering::Relaxed);
        });

        let started = tokio::time::Instant::now();
        let error = install_model(
            directory.path(),
            &model,
            |_| {},
            cancelled,
            local_policy(),
            DownloadTimeouts {
                connect: Duration::from_secs(1),
                request: Duration::from_secs(1),
                idle: Duration::from_secs(1),
            },
        )
        .await
        .unwrap_err();
        let elapsed = started.elapsed();
        server.join().unwrap();

        assert_eq!(error, "Language pack download cancelled.");
        assert!(elapsed < Duration::from_millis(500), "{elapsed:?}");
        assert!(!model_dir(directory.path(), "et").exists());
        assert!(!directory
            .path()
            .join(MODEL_DIRECTORY)
            .join(".et-en.download")
            .exists());
    }

    #[tokio::test]
    async fn removal_is_scoped_and_idempotent() {
        let directory = tempfile::tempdir().unwrap();
        let pack = model_dir(directory.path(), "et");
        std::fs::create_dir_all(&pack).unwrap();
        std::fs::write(pack.join("model.bin"), b"data").unwrap();
        let sibling = directory.path().join("keep-me");
        std::fs::write(&sibling, b"safe").unwrap();

        remove(directory.path(), "et").await.unwrap();
        remove(directory.path(), "et").await.unwrap();

        assert!(!pack.exists());
        assert_eq!(std::fs::read(sibling).unwrap(), b"safe");
    }
}
