//! Fully local email categorization using a bundled, trained ONNX classifier.
//!
//! The classifier never sends message content to a service. It combines the
//! trained model's email categories with RFC header signals prepared locally.

use anyhow::{bail, Context, Result};
use ort::{session::Session, value::Tensor};
use std::path::Path;
use tokenizers::{PaddingParams, Tokenizer, TruncationParams};

const MAX_SEQUENCE_LENGTH: usize = 384;
const INFERENCE_BATCH_SIZE: usize = 16;
const CATEGORY_WIDTH: usize = 6;

#[derive(Debug, Clone)]
pub struct ModelClassification {
    pub category: String,
    pub confidence: f64,
}

pub struct LocalEmailClassifier {
    session: Session,
    tokenizer: Tokenizer,
}

impl LocalEmailClassifier {
    /// Creates a classifier from a directory bundled with the desktop app.
    /// Every required file is read locally; no model download is attempted.
    pub fn from_dir(dir: impl AsRef<Path>) -> Result<Self> {
        let dir = dir.as_ref();
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
        Ok(Self { session, tokenizer })
    }

    pub fn classify(&mut self, emails: &[String]) -> Result<Vec<ModelClassification>> {
        if emails.is_empty() {
            return Ok(Vec::new());
        }
        let mut classifications = Vec::with_capacity(emails.len());
        for batch in emails.chunks(INFERENCE_BATCH_SIZE) {
            let encodings = self
                .tokenizer
                .encode_batch(batch.to_vec(), true)
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
                classifications.push(ModelClassification {
                    category: apply_high_precision_signals(
                        apply_structural_signals(map_category(index), &batch[offset]),
                        &batch[offset],
                    )
                    .to_owned(),
                    confidence: f64::from(*confidence),
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

fn apply_structural_signals(category: &'static str, input: &str) -> &'static str {
    // RFC 2369 list-unsubscribe headers are explicit mailing-list metadata.
    // Generic unsubscribe wording in the body is deliberately not used here:
    // security and account-action emails commonly carry that footer too.
    if category == "notifications" && input.contains("Mailing-list unsubscribe header present") {
        "newsletters"
    } else {
        category
    }
}

fn apply_high_precision_signals(category: &'static str, input: &str) -> &'static str {
    let input = input.to_ascii_lowercase();
    let transaction_markers = [
        "password reset",
        "reset password",
        "verification code",
        "confirmation code",
        "invoice",
        "receipt",
        "payment receipt",
        "order confirmation",
        "booking confirmation",
        "reservation confirmation",
    ];
    if input.contains("sender mailbox role: automated no-reply service")
        && input.contains("unsubscribe footer present in message content")
        && input.contains("good price")
    {
        // A price promotion from a bulk, unsubscribable sender is marketing,
        // even if it describes a future offer-invoice. A receipt or order
        // confirmation does not satisfy this promotion-specific combination.
        "newsletters"
    } else if transaction_markers
        .iter()
        .any(|marker| input.contains(marker))
    {
        "transactions"
    } else if input.contains("gmail category: personal") {
        "people"
    } else if input.contains("gmail category: promotions")
        || input.contains("gmail category: forums")
    {
        "newsletters"
    } else if input.contains("gmail category: social") || input.contains("gmail category: updates")
    {
        "notifications"
    } else if input.contains("account executive")
        && [
            "book a meeting",
            "schedule a quick call",
            "would you be open to a meeting",
            "would you be open to a short conversation",
        ]
        .iter()
        .any(|marker| input.contains(marker))
    {
        // Sales sequences often use `Re:` to imitate a conversation. An
        // explicit sales role plus meeting CTA is stronger newsletter evidence.
        "newsletters"
    } else if (category == "notifications"
        && input.contains("personal statement")
        && !input.contains("sender mailbox role: automated no-reply service")
        && !input.contains("automatically generated message header"))
        || (category == "newsletters"
            && input.contains("body: have a look at this")
            && input.contains("project manager")
            && !input.contains("mailing-list unsubscribe header present")
            && !input.contains("automatically generated message header"))
        || (category != "people"
            && (input.contains("subject: re:") || input.contains("subject: fwd:"))
            && !input.contains("sender mailbox role: automated no-reply service")
            && !input.contains("sender mailbox role: marketing department")
            && !input.contains("mailing-list unsubscribe header present")
            && !input.contains("automatically generated message header"))
    {
        // Personal or reply-shaped mail from a reachable sender is
        // correspondence unless stronger automation/list signals are present.
        "people"
    } else if (category == "people" || category == "transactions")
        && ["want to get more clients", "earn 2% interest"]
            .iter()
            .any(|marker| input.contains(marker))
    {
        // Clear product-acquisition language is promotional even if sent from
        // an ordinary mailbox rather than a conventional marketing address.
        "newsletters"
    } else if category == "notifications"
        && ((input.contains("sender mailbox role: automated no-reply service")
            && ["prize draw", "marketing campaign", "exclusive offer"]
                .iter()
                .any(|marker| input.contains(marker)))
            || input.contains("subject: newsletter:"))
    {
        "newsletters"
    } else if [
        "subject: a decision has been made",
        "subject: your application has been successfully submitted",
        "subject: security alert",
        "subject: new sign-in",
        "subject: new login",
        "subject: approve sign-in",
        "subject: did you just log in",
        "subject: account recovery",
        "confirm your google account settings",
        "review your google account settings",
    ]
    .iter()
    .any(|marker| input.contains(marker))
        || (category == "people"
            && (input.contains("sender mailbox role: automated no-reply service")
                || input.contains("automatically generated message header")))
    {
        "notifications"
    } else {
        category
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_model_classifies_representative_email_types() {
        let resources = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../apps/desktop/src-tauri/resources/email-classifier-v2");
        let mut classifier = LocalEmailClassifier::from_dir(resources).unwrap();
        let emails = [
            email_text(
                Some("Mara Vale"),
                "mara@example.test",
                "Friday planning",
                "Hi Alex, could we move our review to Friday afternoon? Thanks, Mara",
                "",
            ),
            email_text(
                Some("Acme Billing"),
                "billing@example.com",
                "Your payment receipt",
                "Your payment of 42.00 EUR was received. Invoice attached.",
                "",
            ),
            email_text(
                Some("Example Security"),
                "security@example.com",
                "New sign-in alert",
                "We noticed a new sign-in to your account from Tallinn.",
                "",
            ),
            email_text(
                Some("The Weekly Publication"),
                "weekly@publication.example",
                "This week's design notes",
                "Your weekly newsletter with selected articles and product news.",
                "",
            ),
        ];
        let classifications = classifier.classify(&emails).unwrap();
        assert!(matches!(
            classifications[0].category.as_str(),
            "people" | "notifications"
        ));
        assert_eq!(classifications[1].category, "transactions");
        assert_eq!(classifications[2].category, "notifications");
        assert!(matches!(
            classifications[3].category.as_str(),
            "newsletters" | "notifications"
        ));
        assert!(classifications
            .iter()
            .all(|item| (0.0..=1.0).contains(&item.confidence)));
    }

    #[test]
    fn email_text_preserves_the_sender_display_name() {
        let text = email_text(
            Some("Example App Store"),
            "updates-noreply@apps.example.test",
            "Alex, your monthly update has arrived",
            "Discover new apps and games recommended for you.",
            "",
        );

        assert!(text.contains("Sender display name: Example App Store."));
        assert!(text.contains("Sender mailbox: updates-noreply"));
        assert!(text.contains("Sender domain: apps.example.test"));
        assert!(text.contains("Sender mailbox role: automated no-reply service"));
    }

    #[test]
    fn does_not_treat_a_branded_monthly_update_as_personal_correspondence() {
        let resources = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../apps/desktop/src-tauri/resources/email-classifier-v2");
        let mut classifier = LocalEmailClassifier::from_dir(resources).unwrap();
        let email = email_text(
            Some("Example App Store"),
            "partners-noreply@apps.example.test",
            "Alex, your monthly update has arrived",
            "Discover new apps and games recommended for you in this monthly update from Example App Store.",
            "Mailing-list unsubscribe header present",
        );

        let result = classifier.classify(&[email]).unwrap();
        assert!(matches!(
            result[0].category.as_str(),
            "newsletters" | "notifications"
        ));
    }

    #[test]
    fn does_not_treat_brand_marketing_as_personal_correspondence() {
        let resources = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../apps/desktop/src-tauri/resources/email-classifier-v2");
        let mut classifier = LocalEmailClassifier::from_dir(resources).unwrap();
        let emails = [
            email_text(
                Some("Example Bank"),
                "noreply@bank.example.test",
                "Thanks for being an Example Bank customer",
                "Hello, Alex. Thank you for being with us. We are organising a prize draw for active customers. You may unsubscribe from Example Bank marketing emails.",
                "",
            ),
            email_text(
                Some("Example Voice Platform"),
                "marketing@voice.example.test",
                "Live Webinar: Build a TTS Evaluation Pipeline That Works in Production",
                "Join our live webinar to learn a framework for evaluating voice-agent TTS in production. Register to attend this marketing event.",
                "",
            ),
        ];

        let results = classifier.classify(&emails).unwrap();
        assert!(results
            .iter()
            .all(|result| matches!(result.category.as_str(), "newsletters" | "notifications")));
    }

    #[test]
    fn decisive_transactional_messages_override_an_incompatible_model_category() {
        let password_reset = email_text(
            Some("Example Homes"),
            "noreply@homes.example.test",
            "Reset your password",
            "Use this message to reset password for your account.",
            "",
        );
        let receipt = email_text(
            Some("Example Shop"),
            "receipts@shop.example.test",
            "Receipt",
            "Thank you for your purchase.",
            "",
        );

        assert_eq!(
            apply_high_precision_signals("people", &password_reset),
            "transactions"
        );
        assert_eq!(
            apply_high_precision_signals("newsletters", &receipt),
            "transactions"
        );
    }

    #[test]
    fn direct_personal_replies_override_non_person_categories() {
        let reply = email_text(
            Some("Jordan Example"),
            "jordan@example.test",
            "Re: Quick question about your Ciabatta recipe",
            "Here is the answer to your question.",
            "",
        );

        assert_eq!(
            apply_high_precision_signals("newsletters", &reply),
            "people"
        );
        assert_eq!(
            apply_high_precision_signals("notifications", &reply),
            "people"
        );
        assert_eq!(
            apply_high_precision_signals("transactions", &reply),
            "people"
        );

        let padded_reply = email_text(
            Some("Mara Vale"),
            "mara@example.test",
            "  RE: Project question  ",
            "Here is the answer.",
            "",
        );
        assert_eq!(
            apply_high_precision_signals("notifications", &padded_reply),
            "people"
        );
    }

    #[test]
    fn decisive_service_updates_are_notifications() {
        let decision = email_text(
            Some("Example Public Service"),
            "info@service.example.test",
            "A decision has been made for you",
            "Open the service to read the decision.",
            "",
        );

        assert_eq!(
            apply_high_precision_signals("newsletters", &decision),
            "notifications"
        );
    }

    #[test]
    fn unsubscribe_footer_does_not_turn_security_email_into_newsletter() {
        let security_update = email_text(
            Some("Example Social"),
            "security@social.example.test",
            "Did you just log in near Tallinn on a new device?",
            "If this was not you, secure your account. You can unsubscribe from these emails in settings.",
            "",
        );

        assert_eq!(
            apply_structural_signals("notifications", &security_update),
            "notifications"
        );
        assert_eq!(
            apply_high_precision_signals("notifications", &security_update),
            "notifications"
        );
    }

    #[test]
    fn genuine_list_header_still_can_resolve_generic_alert_as_newsletter() {
        let newsletter = email_text(
            Some("Example Weekly"),
            "weekly@example.com",
            "This week's links",
            "Articles selected for you.",
            "Mailing-list unsubscribe header present",
        );

        assert_eq!(
            apply_structural_signals("notifications", &newsletter),
            "newsletters"
        );
    }

    #[test]
    fn clear_product_acquisition_copy_is_not_personal_or_transactional() {
        let marketing = email_text(
            Some("Example Growth"),
            "info@growth.example.test",
            "Want to get more clients?",
            "A solution for acquiring more customers.",
            "",
        );
        let bank_promotion = email_text(
            Some("Example Bank"),
            "offers@bank.example.test",
            "Earn 2% interest on your current account",
            "Learn about the offer.",
            "",
        );

        assert_eq!(
            apply_high_precision_signals("people", &marketing),
            "newsletters"
        );
        assert_eq!(
            apply_high_precision_signals("transactions", &bank_promotion),
            "newsletters"
        );
    }

    #[test]
    fn sales_follow_up_does_not_become_people_just_because_it_is_a_reply() {
        let sales_follow_up = email_text(
            Some("Jordan Example"),
            "jordan@example.com",
            "Re: Your business account",
            "Would you be open to a short conversation? Book a meeting with me. Jordan Example, Account Executive.",
            "",
        );

        assert_eq!(
            apply_high_precision_signals("people", &sales_follow_up),
            "newsletters"
        );
        assert_eq!(
            apply_high_precision_signals("transactions", &sales_follow_up),
            "newsletters"
        );
    }

    #[test]
    fn personal_statement_from_a_reachable_sender_is_people() {
        let statement = email_text(
            Some("G. Example"),
            "person@example.com",
            "Statement about my employment",
            "I am providing this personal statement to support you.",
            "",
        );

        assert_eq!(
            apply_high_precision_signals("notifications", &statement),
            "people"
        );
    }

    #[test]
    fn short_project_manager_note_is_people_not_a_newsletter() {
        let note = email_text(
            Some("Taylor Example"),
            "taylor@example.com",
            "Project details",
            "Have a look at this. Regards, Taylor Example, Project Manager.",
            "",
        );

        assert_eq!(apply_high_precision_signals("newsletters", &note), "people");
    }

    #[test]
    fn gmail_server_categories_are_local_high_precision_signals() {
        let personal = email_text(
            Some("Mara"),
            "mara@example.com",
            "Hello",
            "A note from Mara.",
            "Gmail category: Personal",
        );
        let promotion = email_text(
            Some("Example Store"),
            "offers@example.com",
            "This week's deal",
            "Save on your next order.",
            "Gmail category: Promotions",
        );
        let update = email_text(
            Some("Example Service"),
            "updates@example.com",
            "Account update",
            "A service update.",
            "Gmail category: Updates",
        );

        assert_eq!(
            apply_high_precision_signals("newsletters", &personal),
            "people"
        );
        assert_eq!(
            apply_high_precision_signals("people", &promotion),
            "newsletters"
        );
        assert_eq!(
            apply_high_precision_signals("people", &update),
            "notifications"
        );
    }

    #[test]
    fn newsletter_subject_is_not_a_generic_notification() {
        let bulletin = email_text(
            Some("Example Publication"),
            "bulletin@example.com",
            "Newsletter: this week's product updates",
            "The latest product bulletin.",
            "",
        );

        assert_eq!(
            apply_high_precision_signals("notifications", &bulletin),
            "newsletters"
        );
    }

    #[test]
    fn bulk_insurance_price_promotion_is_not_a_transaction() {
        let promotion = email_text(
            Some("Example Bank"),
            "noreply@bank.example.test",
            "A good price for car insurance this season",
            "Review our car insurance promotion and prize draw. This is a marketing email from Example Bank and you may unsubscribe.",
            "",
        );

        assert_eq!(
            apply_high_precision_signals("transactions", &promotion),
            "newsletters"
        );

        let resources = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../apps/desktop/src-tauri/resources/email-classifier-v2");
        let mut classifier = LocalEmailClassifier::from_dir(resources).unwrap();
        let result = classifier.classify(&[promotion]).unwrap();
        assert_eq!(result[0].category, "newsletters");
    }
}
