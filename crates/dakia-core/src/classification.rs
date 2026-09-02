//! Fully local email categorization using a bundled, trained ONNX classifier.
//!
//! The classifier never sends message content to a service. It combines the
//! trained model's email categories with RFC header signals prepared locally.

use anyhow::{bail, Context, Result};
use ort::{session::Session, value::Tensor};
use sha2::{Digest, Sha256};
use std::path::Path;
use tokenizers::{PaddingParams, Tokenizer, TruncationParams};

const MAX_SEQUENCE_LENGTH: usize = 384;
const INFERENCE_BATCH_SIZE: usize = 16;
const CATEGORY_WIDTH: usize = 6;
const MAX_EVIDENCE_BODY_CHARS: usize = 4_000;

#[derive(Debug, Clone)]
pub struct ModelClassification {
    pub category: String,
    /// Probability for the stored category when it is the model's own winner.
    /// Deterministic policy overrides deliberately have no model confidence.
    pub confidence: Option<f64>,
}

/// The trusted fields that make up one classification request.
///
/// `classification_signals` is deliberately distinct from message content: it
/// contains only RFC/header information extracted by the mail parser. Do not
/// put it into `subject` or `body_text`, and do not derive it by parsing the
/// model prompt. Message authors control the latter two fields.
#[derive(Debug, Clone)]
pub struct EmailClassificationInput {
    pub from_name: Option<String>,
    pub from_address: String,
    pub subject: String,
    pub body_text: String,
    pub classification_signals: String,
    /// Account-local thread evidence: a verified participant relationship or
    /// a message in the user's Sent/thread history. It is not derived from a
    /// sender-controlled Reply-To header.
    pub known_correspondence: bool,
}

impl EmailClassificationInput {
    pub fn new(
        from_name: Option<&str>,
        from_address: &str,
        subject: &str,
        body_text: &str,
        classification_signals: &str,
    ) -> Self {
        Self {
            from_name: from_name.map(str::to_owned),
            from_address: from_address.to_owned(),
            subject: subject.to_owned(),
            body_text: body_text.chars().take(MAX_EVIDENCE_BODY_CHARS).collect(),
            classification_signals: classification_signals.to_owned(),
            known_correspondence: false,
        }
    }

    pub fn with_known_correspondence(mut self, known_correspondence: bool) -> Self {
        self.known_correspondence = known_correspondence;
        self
    }

    fn model_text(&self) -> String {
        email_text(
            self.from_name.as_deref(),
            &self.from_address,
            &self.subject,
            &self.body_text,
            &self.classification_signals,
        )
    }
}

pub struct LocalEmailClassifier {
    session: Session,
    tokenizer: Tokenizer,
    revision: String,
}

impl LocalEmailClassifier {
    /// Creates a classifier from a directory bundled with the desktop app.
    /// Every required file is read locally; no model download is attempted.
    pub fn from_dir(dir: impl AsRef<Path>) -> Result<Self> {
        let dir = dir.as_ref();
        let manifest = read_asset(dir, "MANIFEST.json")?;
        let revision = Sha256::digest(&manifest)
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        let mut tokenizer =
            Tokenizer::from_bytes(read_asset(dir, "tokenizer.json")?).map_err(|error| {
                anyhow::anyhow!("could not load bundled email-classifier tokenizer: {error}")
            })?;
        tokenizer
            .with_truncation(Some(TruncationParams {
                max_length: MAX_SEQUENCE_LENGTH,
                ..Default::default()
            }))
            .map_err(|error| {
                anyhow::anyhow!(
                    "could not configure email-classifier tokenizer truncation: {error}"
                )
            })?;
        tokenizer.with_padding(Some(PaddingParams::default()));
        let session = Session::builder()
            .context("could not create bundled email-classifier runtime")?
            .commit_from_file(dir.join("model.onnx"))
            .context("could not load bundled ONNX email classifier")?;
        Ok(Self {
            session,
            tokenizer,
            revision,
        })
    }

    /// Stable revision of the complete bundled model manifest. Storage keeps
    /// it separately from deterministic policy so replacing model assets
    /// automatically requeues model-owned decisions.
    pub fn revision(&self) -> &str {
        &self.revision
    }

    pub fn classify(
        &mut self,
        emails: &[EmailClassificationInput],
    ) -> Result<Vec<ModelClassification>> {
        if emails.is_empty() {
            return Ok(Vec::new());
        }
        let mut classifications = Vec::with_capacity(emails.len());
        for batch in emails.chunks(INFERENCE_BATCH_SIZE) {
            let model_inputs: Vec<String> = batch
                .iter()
                .map(EmailClassificationInput::model_text)
                .collect();
            let encodings = self
                .tokenizer
                .encode_batch(model_inputs, true)
                .map_err(|error| {
                    anyhow::anyhow!("could not tokenize emails for local classification: {error}")
                })?;
            let sequence_length = encodings
                .first()
                .map(|encoding| encoding.get_ids().len())
                .unwrap_or_default();
            if sequence_length == 0 {
                bail!("email classifier tokenizer produced an empty sequence");
            }
            let input_ids: Vec<i64> = encodings
                .iter()
                .flat_map(|encoding| encoding.get_ids().iter().copied().map(i64::from))
                .collect();
            let attention_mask: Vec<i64> = encodings
                .iter()
                .flat_map(|encoding| encoding.get_attention_mask().iter().copied().map(i64::from))
                .collect();
            let outputs = self
                .session
                .run(ort::inputs! {
                    "input_ids" => Tensor::<i64>::from_array(([batch.len(), sequence_length], input_ids))?,
                    "attention_mask" => Tensor::<i64>::from_array(([batch.len(), sequence_length], attention_mask))?,
                })
                .context("could not run bundled ONNX email classifier")?;
            let (_, probabilities) = outputs["category_probs"]
                .try_extract_tensor::<f32>()
                .context("bundled classifier returned invalid category probabilities")?;
            if probabilities.len() != batch.len() * CATEGORY_WIDTH {
                bail!("bundled classifier returned an unexpected category shape");
            }
            for (offset, scores) in probabilities.chunks_exact(CATEGORY_WIDTH).enumerate() {
                let (index, confidence) = scores
                    .iter()
                    .enumerate()
                    .max_by(|left, right| left.1.total_cmp(right.1))
                    .expect("classifier category output has a fixed nonzero width");
                let model_category = map_category(index);
                let model_confidence = f64::from(*confidence);
                let category = resolve_category(model_category, model_confidence, &batch[offset]);
                classifications.push(ModelClassification {
                    category: category.to_owned(),
                    confidence: (category == model_category).then_some(model_confidence),
                });
            }
        }
        Ok(classifications)
    }
}

pub fn email_text(
    from_name: Option<&str>,
    from_address: &str,
    subject: &str,
    body_text: &str,
    classification_signals: &str,
) -> String {
    let subject = subject.trim();
    let body = body_text
        .split_whitespace()
        .filter(|token| !is_url(token))
        .collect::<Vec<_>>()
        .join(" ");
    let has_unsubscribe_footer = body.to_ascii_lowercase().contains("unsubscribe");
    // The sender, subject, and structural headers are more reliable category
    // evidence than a long, template-heavy marketing body. A concise opening
    // still gives the model intent while avoiding link and boilerplate noise.
    let body: String = body.chars().take(800).collect();
    let mut signals = classification_signals.trim().to_owned();
    if has_unsubscribe_footer {
        if !signals.is_empty() {
            signals.push('\n');
        }
        signals.push_str("Unsubscribe footer present in message content");
    }
    let display_name = from_name
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .unwrap_or("(not provided)");
    let (mailbox, domain) = from_address
        .rsplit_once('@')
        .map_or(("(not provided)", "(not provided)"), |(mailbox, domain)| {
            (mailbox, domain)
        });
    let sender_role = sender_mailbox_role(mailbox);
    let metadata = if signals.is_empty() {
        format!("Sender display name: {display_name}. Sender mailbox: {mailbox}. Sender domain: {domain}. {sender_role}")
    } else {
        format!("Sender display name: {display_name}. Sender mailbox: {mailbox}. Sender domain: {domain}. {sender_role}Structural email signals: {signals}")
    };
    format!("Subject: {subject}\n\nBody: {body}\n\nMetadata: {metadata}")
}

fn sender_mailbox_role(mailbox: &str) -> &'static str {
    let mailbox = mailbox.to_ascii_lowercase();
    if mailbox.contains("marketing") {
        "Sender mailbox role: marketing department\n"
    } else if mailbox.contains("newsletter") || mailbox.contains("news") {
        "Sender mailbox role: newsletter or publications team\n"
    } else if mailbox.contains("noreply") || mailbox.contains("no-reply") {
        "Sender mailbox role: automated no-reply service\n"
    } else {
        ""
    }
}

fn is_url(token: &&str) -> bool {
    let token = token
        .trim_matches(|character: char| matches!(character, '(' | ')' | '[' | ']' | ',' | '.'));
    token.contains("://") || token.starts_with("www.") || token.starts_with("mailto:")
}

fn read_asset(dir: &Path, name: &str) -> Result<Vec<u8>> {
    std::fs::read(dir.join(name))
        .with_context(|| format!("missing bundled classifier asset: {name}"))
}

fn map_category(index: usize) -> &'static str {
    match index {
        0 | 4 => "notifications", // ALERT or SOCIAL
        1 | 3 => "newsletters",   // NEWSLETTER or PROMOTIONAL
        2 => "people",            // PERSONAL
        5 => "transactions",      // TRANSACTION
        _ => unreachable!("classifier category width is fixed"),
    }
}

/// Resolves the model result with evidence that is more reliable than natural
/// language classification. The order is intentional:
///
/// 1. Verified person-to-person correspondence is preserved.
/// 2. RFC list/bulk metadata outranks sender-authored lexical phrases.
/// 3. Concrete transaction language outranks weak model/provider hints.
/// 4. Explicit promotion evidence defeats reply-shaped sales sequences.
/// 5. Automated or organisation-branded mail that the model calls People is a
///    notification. Gmail categories are merely a final fallback.
fn resolve_category(
    model_category: &'static str,
    _model_confidence: f64,
    input: &EmailClassificationInput,
) -> &'static str {
    let evidence = ClassificationEvidence::from(input);

    if evidence.is_known_correspondence() {
        return "people";
    }
    if evidence.has_list_or_bulk_evidence() {
        return "newsletters";
    }
    if evidence.has_strong_transaction_intent() {
        return "transactions";
    }
    if evidence.has_promotion_evidence() {
        return "newsletters";
    }
    if evidence.has_service_notification_intent() {
        return "notifications";
    }
    if model_category == "people" && (evidence.is_automated() || evidence.is_organisation()) {
        return "notifications";
    }

    // People is precision-first. A large model score or human-sounding prose
    // cannot establish a relationship: transactional brands and cold sales
    // frequently receive a confident PERSONAL output. Only verified,
    // account-local correspondence above can elevate mail into People.
    if model_category == "people" {
        return "other";
    }

    // Provider categories are useful hints, but must never outrank the
    // independently observed message/header evidence above.
    match evidence.gmail_category() {
        // Personal is deliberately not an elevation signal. Gmail can label
        // an automated order or account message Personal, and it cannot prove
        // a relationship with the sender.
        Some("personal") => model_category,
        Some("promotions") | Some("forums") => "newsletters",
        Some("social") | Some("updates") => "notifications",
        _ => model_category,
    }
}

struct ClassificationEvidence {
    subject: String,
    body: String,
    display_name: String,
    mailbox: String,
    domain: String,
    signals: Vec<String>,
    known_correspondence: bool,
}

impl From<&EmailClassificationInput> for ClassificationEvidence {
    fn from(input: &EmailClassificationInput) -> Self {
        let (mailbox, domain) = input.from_address.rsplit_once('@').unwrap_or(("", ""));
        Self {
            subject: normalise_message_text(&input.subject),
            body: normalise_message_text(&input.body_text),
            display_name: input
                .from_name
                .as_deref()
                .unwrap_or_default()
                .to_lowercase(),
            mailbox: mailbox.to_lowercase(),
            domain: domain.to_lowercase(),
            // Only the dedicated parser-produced signal field is trusted for
            // RFC/Gmail evidence. Subject and body text can imitate it.
            signals: input
                .classification_signals
                .lines()
                .map(|signal| signal.trim().to_lowercase())
                .filter(|signal| !signal.is_empty())
                .collect(),
            known_correspondence: input.known_correspondence,
        }
    }
}

impl ClassificationEvidence {
    fn message_contains_any(&self, markers: &[&str]) -> bool {
        markers.iter().any(|marker| {
            let marker = marker.to_lowercase();
            self.subject.contains(&marker) || self.body.contains(&marker)
        })
    }

    fn has_signal(&self, signal: &str) -> bool {
        self.signals.iter().any(|candidate| candidate == signal)
    }

    fn has_strong_transaction_intent(&self) -> bool {
        self.message_contains_any(&[
            "password reset",
            "reset password",
            "verification code",
            "confirmation code",
            "payment receipt",
            "payment failed",
            "payment failure",
            "payment declined",
            "order confirmation",
            "booking confirmation",
            "reservation confirmation",
            "viga maksmisel",
            "makse ebaõnnestus",
            "makse on vastu võetud",
            "tellimuse kinnit",
            "tellimus on saadetud",
            "bestellbestätigung",
            "bestellung wurde versandt",
            "zahlung fehlgeschlagen",
        ]) || self.message_contains_any(&["your invoice", "invoice attached", "your receipt"])
            || (self.has_business_sender_evidence()
                && self.message_contains_any(&[
                    "dispatched",
                    "has shipped",
                    "shipment",
                    "delivery confirmation",
                    "delivery status",
                ]))
    }

    fn has_list_or_bulk_evidence(&self) -> bool {
        self.has_signal("mailing-list unsubscribe header present")
            || self.has_signal("mailing-list identifier header present")
            || self.has_signal("bulk-mail precedence header")
            || self.has_signal("mailing-list precedence header")
    }

    fn has_promotion_evidence(&self) -> bool {
        self.message_contains_any(&[
            "newsletter:",
            "marketing campaign",
            "exclusive offer",
            "prize draw",
            "want to get more clients",
            "earn 2% interest",
            "live webinar",
        ]) || (self.message_contains_any(&["kasvata"])
            && self.message_contains_any(&["sissetulek"]))
            || (self.message_contains_any(&["account executive"])
                && self.message_contains_any(&[
                    "book a meeting",
                    "schedule a quick call",
                    "would you be open to a meeting",
                    "would you be open to a short conversation",
                ]))
    }

    fn is_automated(&self) -> bool {
        self.has_signal("automatically generated message header")
            || self.has_signal("automatically replied message header")
            || [
                "noreply",
                "no-reply",
                "do-not-reply",
                "donotreply",
                "mailer-daemon",
            ]
            .iter()
            .any(|marker| self.mailbox.contains(marker))
    }

    fn is_organisation(&self) -> bool {
        let domain_labels = self
            .domain
            .split('.')
            .map(|label| {
                label
                    .chars()
                    .filter(|character| character.is_ascii_alphanumeric())
                    .collect::<String>()
            })
            .filter(|label| {
                label.len() >= 4
                    && !matches!(
                        label.as_str(),
                        "email"
                            | "mail"
                            | "news"
                            | "info"
                            | "notify"
                            | "updates"
                            | "notifications"
                            | "noreply"
                            | "example"
                            | "test"
                            | "localhost"
                    )
            })
            .collect::<Vec<_>>();
        let display_compact = self
            .display_name
            .chars()
            .filter(|character| character.is_ascii_alphanumeric())
            .collect::<String>();
        let display_words = self
            .display_name
            .split(|character: char| !character.is_ascii_alphanumeric())
            .filter(|word| word.len() >= 3)
            .collect::<Vec<_>>();
        let brand_shaped_display = display_words.len() == 1 || self.display_name.contains('.');
        brand_shaped_display
            && domain_labels.iter().any(|domain_label| {
                display_words.iter().any(|word| *word == domain_label)
                    || display_compact.contains(domain_label)
            })
    }

    fn is_known_correspondence(&self) -> bool {
        self.known_correspondence
            && self.has_person_shaped_identity()
            && !self.has_role_mailbox()
            && !self.is_automated()
            && !self.has_list_or_bulk_evidence()
    }

    fn has_role_mailbox(&self) -> bool {
        const ROLES: &[&str] = &[
            "admin",
            "billing",
            "contact",
            "customer",
            "customercare",
            "customerservice",
            "events",
            "hello",
            "help",
            "helpdesk",
            "info",
            "marketing",
            "news",
            "newsletter",
            "notifications",
            "notify",
            "orders",
            "security",
            "sales",
            "service",
            "support",
            "supportdesk",
            "team",
            "updates",
        ];
        self.mailbox
            .split(|character: char| !character.is_ascii_alphanumeric())
            .any(|part| ROLES.contains(&part))
    }

    fn has_person_shaped_identity(&self) -> bool {
        const ORGANISATION_WORDS: &[&str] = &[
            "ag",
            "as",
            "association",
            "bank",
            "bv",
            "company",
            "corp",
            "corporation",
            "foundation",
            "gmbh",
            "group",
            "inc",
            "limited",
            "llc",
            "ltd",
            "marketing",
            "newsletter",
            "ou",
            "oü",
            "oy",
            "plc",
            "sa",
            "shop",
            "store",
            "support",
            "team",
        ];
        if self.display_name.contains('.') {
            return false;
        }
        let words = self
            .display_name
            .split(|character: char| !character.is_alphabetic())
            .filter(|word| word.chars().count() >= 2)
            .collect::<Vec<_>>();
        words.len() >= 2 && !words.iter().any(|word| ORGANISATION_WORDS.contains(word))
    }

    fn has_business_sender_evidence(&self) -> bool {
        self.is_automated() || self.is_organisation() || self.has_role_mailbox()
    }

    fn has_service_notification_intent(&self) -> bool {
        self.message_contains_any(&[
            "a decision has been made",
            "application has been successfully submitted",
            "security alert",
            "new sign-in",
            "new login",
            "approve sign-in",
            "did you just log in",
            "account recovery",
            "confirm your google account settings",
            "review your google account settings",
        ]) || ((self.is_automated() || self.is_organisation())
            && self.message_contains_any(&["welcome", "tere tulemast", "konto", "account update"]))
    }

    fn gmail_category(&self) -> Option<&str> {
        if self.has_signal("gmail category: personal") {
            Some("personal")
        } else if self.has_signal("gmail category: promotions") {
            Some("promotions")
        } else if self.has_signal("gmail category: social") {
            Some("social")
        } else if self.has_signal("gmail category: updates") {
            Some("updates")
        } else if self.has_signal("gmail category: forums") {
            Some("forums")
        } else {
            None
        }
    }
}

fn normalise_message_text(value: &str) -> String {
    value
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

#[cfg(test)]
mod resolver_tests {
    use super::*;

    fn email(
        name: Option<&str>,
        address: &str,
        subject: &str,
        body: &str,
        signals: &str,
    ) -> EmailClassificationInput {
        EmailClassificationInput::new(name, address, subject, body, signals)
    }

    #[test]
    fn transaction_intent_beats_model_and_gmail_evidence() {
        let amazon = email(
            Some("Amazon.de"),
            "shipment-tracking@amazon.de",
            "Dispatched: ‘BENFEI 6-in-1 USB C HUB…’",
            "Your order has been dispatched and is on its way.",
            "Gmail category: Personal",
        );
        let nojus_payment = email(
            Some("Nojus.ee"),
            "noreply@nojus.ee",
            "[NOJUS.EE] VIGA MAKSMISEL",
            "TEIE MAKSE EBAÕNNESTUS. PALUN PROOVIGE UUESTI.",
            "Gmail category: Personal",
        );
        let nojus_order = email(
            Some("Nojus.ee"),
            "orders@nojus.ee",
            "[Nojus.ee] Tellimuse kinnitamine",
            "Teie tellimus on vastu võetud.",
            "Gmail category: Personal",
        );
        let nojus_accepted_payment = email(
            Some("Nojus.ee"),
            "noreply@nojus.ee",
            "[Nojus.ee] Makse on vastu võetud",
            "Teie makse on vastu võetud.",
            "Gmail category: Personal",
        );

        assert_eq!(resolve_category("people", 0.99, &amazon), "transactions");
        assert_eq!(
            resolve_category("newsletters", 0.87, &nojus_payment),
            "transactions"
        );
        assert_eq!(
            resolve_category("people", 0.98, &nojus_order),
            "transactions"
        );
        assert_eq!(
            resolve_category("people", 0.98, &nojus_accepted_payment),
            "transactions"
        );
    }

    #[test]
    fn organisation_welcome_is_a_notification_not_people() {
        let welcome = email(
            Some("Nojus.ee"),
            "info@nojus.ee",
            "[Nojus.ee] Tere tulemast!",
            "Tere tulemast Nojus.ee kontole.",
            "Gmail category: Personal",
        );

        assert_eq!(resolve_category("people", 0.96, &welcome), "notifications");
    }

    #[test]
    fn real_bulk_headers_are_authoritative_newsletter_evidence() {
        let bigbank = email(
            Some("Bigbank"),
            "news@bigbank.ee",
            "Kasvata Bigbankis enda sissetulekud",
            "Loe selle kuu pakkumist.",
            "Mailing-list identifier header present\nBulk-mail precedence header\nGmail category: Personal",
        );

        assert_eq!(resolve_category("people", 0.98, &bigbank), "newsletters");

        let acquisition_without_headers = email(
            Some("Bigbank"),
            "info@bigbank.ee",
            "Kasvata Bigbankis enda sissetulekud",
            "Tutvu pakkumisega.",
            "",
        );
        assert_eq!(
            resolve_category("people", 0.97, &acquisition_without_headers),
            "newsletters"
        );

        let branded_subdomain = email(
            Some("Bigbank"),
            "info@email.bigbank.ee",
            "Konto teade",
            "Vaata oma kontot.",
            "",
        );
        assert_eq!(
            resolve_category("people", 0.98, &branded_subdomain),
            "notifications"
        );

        let shipment_trends = email(
            Some("Logistics Weekly"),
            "newsletter@logistics.example",
            "Shipment trends for growing retailers",
            "This week's industry analysis.",
            "Mailing-list unsubscribe header present",
        );
        assert_eq!(
            resolve_category("transactions", 0.98, &shipment_trends),
            "newsletters"
        );
    }

    #[test]
    fn gmail_personal_is_a_weak_hint_not_an_override() {
        let automated = email(
            Some("Example Service"),
            "noreply@service.example",
            "Your account update",
            "Your preferences have been changed.",
            "Gmail category: Personal",
        );
        let unknown = email(
            None,
            "someone@personal.example",
            "A note",
            "Just checking in.",
            "Gmail category: Personal",
        );

        assert_eq!(
            resolve_category("people", 0.91, &automated),
            "notifications"
        );
        assert_eq!(
            resolve_category("newsletters", 0.54, &unknown),
            "newsletters"
        );
    }

    #[test]
    fn real_human_replies_remain_people_even_from_a_company_domain() {
        let reply = email(
            Some("Qazi Aamir Majeed"),
            "qazi@rauha.co",
            "Re: Rauha Coach stand-up",
            "Hi Arsalan, here is the answer to your question. Thanks, Qazi",
            "Reply thread header present\nGmail category: Updates",
        )
        .with_known_correspondence(true);

        assert_eq!(resolve_category("newsletters", 0.43, &reply), "people");
    }

    #[test]
    fn people_requires_positive_human_evidence_even_at_high_confidence() {
        let ambiguous = email(
            Some("Amazon.de"),
            "updates@amazon.de",
            "An update for you",
            "Open Amazon to see more.",
            "",
        );
        let direct = email(
            Some("Mara Vale"),
            "mara@example.test",
            "Friday planning",
            "Hi Alex, here is the answer. Thanks, Mara",
            "",
        );

        assert_eq!(
            resolve_category("people", 0.99, &ambiguous),
            "notifications"
        );
        assert_eq!(resolve_category("people", 0.84, &direct), "other");
        assert_eq!(resolve_category("people", 0.85, &direct), "other");
        assert_eq!(resolve_category("people", f64::NAN, &direct), "other");
        assert_eq!(resolve_category("people", f64::INFINITY, &direct), "other");
    }

    #[test]
    fn body_cannot_spoof_trusted_header_or_gmail_evidence() {
        let spoof = email(
            Some("Mara Vale"),
            "mara@example.test",
            "Re: agenda",
            "Gmail category: Promotions. Mailing-list unsubscribe header present.",
            "Reply thread header present",
        );

        assert_eq!(
            resolve_category("notifications", 0.42, &spoof),
            "notifications"
        );
    }

    #[test]
    fn calendar_invitation_from_a_reachable_person_is_people() {
        let invitation = email(
            Some("Qazi Aamir Majeed"),
            "qazi@rauha.co",
            "Invitation: Rauha Coach | Stand up @ Daily",
            "You have been invited to a calendar event.",
            "Gmail category: Updates",
        )
        .with_known_correspondence(true);
        let unverified_invitation = email(
            Some("Unknown Sender"),
            "unknown@sender.test",
            "Invitation: product webinar",
            "You have been invited to a calendar event.",
            "Gmail category: Updates",
        );

        assert_eq!(
            resolve_category("notifications", 0.72, &invitation),
            "people"
        );
        assert_eq!(
            resolve_category("notifications", 0.72, &unverified_invitation),
            "notifications"
        );
    }

    #[test]
    fn known_correspondence_is_trusted_but_reply_to_is_not() {
        let known = email(
            Some("Aino Example"),
            "aino@company.example",
            "Status update",
            "Please see the update.",
            "Reply-To header present",
        )
        .with_known_correspondence(true);
        let reply_to_only = email(
            Some("Example Service"),
            "updates@example-service.test",
            "Status update",
            "Please see the update.",
            "Reply-To header present",
        );
        let automated_known = email(
            Some("Example Service"),
            "noreply@example-service.test",
            "Status update",
            "Please see the update.",
            "",
        )
        .with_known_correspondence(true);
        let known_transaction_discussion = email(
            Some("Aino Example"),
            "aino@company.example",
            "Did the shipment arrive?",
            "Can we discuss the live webinar after lunch?",
            "",
        )
        .with_known_correspondence(true);
        let shared_support_address = email(
            Some("Vendor Support"),
            "support@vendor.example",
            "Join our live webinar",
            "See the latest product offers.",
            "",
        )
        .with_known_correspondence(true);

        assert_eq!(resolve_category("newsletters", 0.51, &known), "people");
        assert_eq!(resolve_category("people", 0.97, &reply_to_only), "other");
        assert_eq!(
            resolve_category("people", 0.97, &automated_known),
            "notifications"
        );
        assert_eq!(
            resolve_category("transactions", 0.97, &known_transaction_discussion),
            "people"
        );
        assert_eq!(
            resolve_category("people", 0.97, &shared_support_address),
            "newsletters"
        );
    }

    #[test]
    fn precedence_and_automation_exclusions_are_stable() {
        let all_evidence = email(
            Some("Amazon.de"),
            "noreply@amazon.de",
            "  DISPATCHED:  order 123 ",
            "Your shipment is ready.",
            "Mailing-list unsubscribe header present\nAutomatically generated message header",
        )
        .with_known_correspondence(true);
        let no_reply_reply = email(
            Some("Example Service"),
            "no-reply@example-service.test",
            "RE: Your account",
            "This is an automatically generated response.",
            "",
        );
        let list_reply = email(
            Some("Example List"),
            "list@example.test",
            "Re: Weekly discussion",
            "A digest.",
            "Mailing-list precedence header",
        )
        .with_known_correspondence(true);

        assert_eq!(
            resolve_category("people", 0.99, &all_evidence),
            "newsletters"
        );
        assert_eq!(
            resolve_category("people", 0.99, &no_reply_reply),
            "notifications"
        );
        assert_eq!(resolve_category("people", 0.99, &list_reply), "newsletters");
    }

    #[test]
    fn reply_subject_without_trusted_thread_context_cannot_elevate_people() {
        let sales_reply = email(
            Some("Pat Example"),
            "pat@company.test",
            "Re: a quick opportunity",
            "I would like to sell you something.",
            "",
        );
        let synthetic_thread_reply = email(
            Some("Pat Example"),
            "pat@company.test",
            "Re: a quick opportunity",
            "I would like to sell you something.",
            "Reply thread header present",
        );
        let invoice_tips = email(
            Some("Example Publication"),
            "bulletin@example.test",
            "Invoice software tips for teams",
            "This week's practical newsletter.",
            "Mailing-list unsubscribe header present",
        );
        let order_workflow = email(
            Some("Example Publication"),
            "bulletin@example.test",
            "Improve your order workflow",
            "This week's practical newsletter.",
            "Mailing-list unsubscribe header present",
        );
        let first_contact = email(
            Some("Pat Example"),
            "pat@company.test",
            "Question about our project",
            "Can you confirm the shipment arrived before our meeting?",
            "",
        );
        let first_contact_dispatched_documents = email(
            Some("Pat Example"),
            "pat@company.test",
            "Documents for our review",
            "I dispatched the documents yesterday; can we review them tomorrow?",
            "",
        );

        assert_eq!(resolve_category("people", 0.99, &sales_reply), "other");
        assert_eq!(
            resolve_category("people", 0.99, &synthetic_thread_reply),
            "other"
        );
        assert_eq!(
            resolve_category("people", 0.99, &invoice_tips),
            "newsletters"
        );
        assert_eq!(
            resolve_category("people", 0.99, &order_workflow),
            "newsletters"
        );
        assert_eq!(resolve_category("people", 0.99, &first_contact), "other");
        assert_eq!(
            resolve_category("people", 0.99, &first_contact_dispatched_documents),
            "other"
        );
    }

    #[test]
    fn repeated_transaction_templates_are_consistent_despite_case_and_whitespace() {
        let first = email(
            Some("Amazon.de"),
            "shipment-tracking@amazon.de",
            "Dispatched: order 1",
            "Your shipment is on its way.",
            "",
        );
        let second = email(
            Some("Amazon.de"),
            "shipment-tracking@amazon.de",
            "  dIsPaTcHeD:   order 2 ",
            "Your   shipment is on its way.",
            "Gmail category: Personal",
        );

        assert_eq!(resolve_category("people", 0.91, &first), "transactions");
        assert_eq!(resolve_category("people", 0.91, &second), "transactions");
    }

    #[test]
    fn classification_input_bounds_sender_controlled_body_work() {
        let body = "õ".repeat(MAX_EVIDENCE_BODY_CHARS + 10_000);
        let input = email(
            Some("Example Sender"),
            "sender@example.test",
            "A bounded note",
            &body,
            "",
        );

        assert_eq!(input.body_text.chars().count(), MAX_EVIDENCE_BODY_CHARS);
    }
}

#[cfg(test)]
mod model_integration_tests {
    use super::*;

    fn classifier() -> LocalEmailClassifier {
        let resources = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../apps/desktop/src-tauri/resources/email-classifier-v2");
        LocalEmailClassifier::from_dir(resources).unwrap()
    }

    #[test]
    fn model_category_mapping_covers_every_output() {
        assert_eq!(map_category(0), "notifications");
        assert_eq!(map_category(1), "newsletters");
        assert_eq!(map_category(2), "people");
        assert_eq!(map_category(3), "newsletters");
        assert_eq!(map_category(4), "notifications");
        assert_eq!(map_category(5), "transactions");
    }

    #[test]
    fn local_model_and_policy_classify_the_production_regression_corpus() {
        let messages = [
            EmailClassificationInput::new(
                Some("Amazon.de"),
                "versandbestaetigung@amazon.de",
                "Dispatched: ‘BENFEI 6-in-1 USB C HUB…’",
                "",
                "",
            ),
            EmailClassificationInput::new(
                Some("Nojus.ee"),
                "seklos@nojus.lt",
                "[Nojus.ee] Viga maksmisel",
                "",
                "Reply-To header present",
            ),
            EmailClassificationInput::new(
                Some("Nojus.ee"),
                "seklos@nojus.lt",
                "[Nojus.ee] Tere tulemast!",
                "",
                "Reply-To header present",
            ),
            EmailClassificationInput::new(
                Some("Bigbank"),
                "info@email.bigbank.ee",
                "Kasvata Bigbankis enda sissetulekut",
                "",
                "Mailing-list unsubscribe header present\nReply-To header present",
            ),
            EmailClassificationInput::new(
                Some("Qazi Aamir Majeed"),
                "qazi@rauha.co",
                "Invitation: Rauha Coach | Stand up @ Daily",
                "You have been invited to a calendar event.",
                "",
            )
            .with_known_correspondence(true),
        ];

        let results = classifier().classify(&messages).unwrap();
        assert_eq!(
            results
                .iter()
                .map(|result| result.category.as_str())
                .collect::<Vec<_>>(),
            [
                "transactions",
                "transactions",
                "notifications",
                "newsletters",
                "people",
            ]
        );
        assert!(results.iter().all(|result| result
            .confidence
            .is_none_or(|confidence| (0.0..=1.0).contains(&confidence))));
    }

    #[test]
    fn model_and_policy_preserve_representative_categories() {
        let messages = [
            EmailClassificationInput::new(
                Some("Mara Vale"),
                "mara@example.test",
                "Friday planning",
                "Hi Alex, could we move our review to Friday afternoon? Thanks, Mara",
                "",
            )
            .with_known_correspondence(true),
            EmailClassificationInput::new(
                Some("Acme Billing"),
                "billing@example.com",
                "Your payment receipt",
                "Your payment of 42.00 EUR was received. Invoice attached.",
                "",
            ),
            EmailClassificationInput::new(
                Some("Example Security"),
                "security@example.com",
                "New sign-in alert",
                "We noticed a new sign-in to your account from Tallinn.",
                "",
            ),
            EmailClassificationInput::new(
                Some("The Weekly Publication"),
                "weekly@publication.example",
                "This week's design notes",
                "Your weekly newsletter with selected articles and product news.",
                "Mailing-list unsubscribe header present",
            ),
        ];

        let results = classifier().classify(&messages).unwrap();
        assert_eq!(results[0].category, "people");
        assert_eq!(results[1].category, "transactions");
        assert_eq!(results[2].category, "notifications");
        assert_eq!(results[3].category, "newsletters");
    }
}
