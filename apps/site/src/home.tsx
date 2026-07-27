import { m as motion, useReducedMotion } from "motion/react";
import {
  IconArrowRight,
  IconBrandApple,
  IconCommand,
  IconFile,
  IconGitBranch,
  IconLock,
  IconLanguage,
  IconSearch,
  IconSparkles,
  IconTerminal2,
} from "@tabler/icons-react";
import {
  CategoryScene,
  CliScene,
  HeroControlCenter,
  IdleNotificationScene,
  PrivacyScene,
  SearchScene,
  TranslationScene,
  UnifiedScene,
  UnsubscribeScene,
} from "./scenes";

type HomeProps = {
  Link: React.ComponentType<{
    to: string;
    className?: string;
    children: React.ReactNode;
  }>;
  DownloadButton: React.ComponentType<{ compact?: boolean }>;
};

const providers = [
  "Gmail",
  "Outlook",
  "Fastmail",
  "iCloud",
  "Yahoo",
  "Zoho",
  "Migadu",
  "Any IMAP",
];

export const offlineTranslationStory = {
  title: "Translate the whole email. Send nothing to the cloud.",
  description:
    "Dakia detects supported languages and translates the subject and formatted message in place. Download a verified language pack once, then translate offline whenever you need it.",
  bullets: [
    "Email content never leaves your device",
    "Preserves the original message layout",
    "One click to show the original",
  ],
} as const;

export const openSourceStory = {
  hero: "Dakia is a free, open-source desktop app that brings every account into one fast command center—without putting your mail on someone else’s servers or another subscription on your card. Even email translation runs privately on your Mac and keeps working offline.",
  title: "Modern email, local by default and open by design.",
  description:
    "Dakia’s source code is public, so anyone can inspect how it handles mail, report an issue, or help improve the client.",
} as const;

function Reveal({
  children,
  className = "",
}: {
  children: React.ReactNode;
  className?: string;
}) {
  const reduce = useReducedMotion();
  return (
    <motion.div
      className={className}
      initial={false}
      whileInView={reduce ? undefined : { y: [12, 0] }}
      viewport={{ once: true, amount: 0.18 }}
      transition={{ duration: 0.65, ease: [0.16, 1, 0.3, 1] }}
    >
      {children}
    </motion.div>
  );
}

function FeatureStory({
  title,
  description,
  bullets,
  scene,
  reversed = false,
}: {
  title: string;
  description: string;
  bullets: string[];
  scene: React.ReactNode;
  reversed?: boolean;
}) {
  return (
    <article
      className={`feature-story ${reversed ? "feature-story-reversed" : ""}`}
    >
      <Reveal className="feature-story-copy">
        <h3>{title}</h3>
        <p>{description}</p>
        <ul>
          {bullets.map((bullet) => (
            <li key={bullet}>{bullet}</li>
          ))}
        </ul>
      </Reveal>
      <Reveal className="feature-story-scene">{scene}</Reveal>
    </article>
  );
}

function CompactFeature({
  title,
  description,
  bullets,
  scene,
  featured = false,
}: {
  title: string;
  description: string;
  bullets: string[];
  scene: React.ReactNode;
  featured?: boolean;
}) {
  return (
    <article
      className={`compact-feature ${featured ? "compact-feature-featured" : ""}`}
    >
      <Reveal className="compact-feature-copy">
        <h3>{title}</h3>
        <p>{description}</p>
        <ul>
          {bullets.map((bullet) => (
            <li key={bullet}>{bullet}</li>
          ))}
        </ul>
      </Reveal>
      <Reveal className="compact-feature-scene">{scene}</Reveal>
    </article>
  );
}

export function Home({ Link, DownloadButton }: HomeProps) {
  const reduce = useReducedMotion();
  return (
    <main>
      <section className="hero">
        <div className="hero-grid shell-wide">
          <div className="hero-copy">
            <h1>
              Every inbox.
              <br />
              <em>One command center.</em>
            </h1>
            <p className="hero-summary">{openSourceStory.hero}</p>
            <div className="hero-actions">
              <DownloadButton />
              <Link to="/#features" className="button button-secondary">
                See how it works <IconArrowRight size={18} />
              </Link>
            </div>
          </div>
          <HeroControlCenter />
        </div>
        <div
          className="provider-strip shell-wide"
          aria-label="Supported email providers"
        >
          <p>One home for</p>
          <div>
            {providers.map((provider) => (
              <span key={provider}>{provider}</span>
            ))}
          </div>
        </div>
      </section>

      <section className="manifesto-section">
        <Reveal className="shell manifesto-layout">
          <h2>{openSourceStory.title}</h2>
          <p>{openSourceStory.description}</p>
        </Reveal>
      </section>

      <section className="feature-theatre" id="features">
        <div className="shell">
          <div className="section-heading">
            <h2>
              Less switching.
              <br />
              More getting things done.
            </h2>
          </div>
          <FeatureStory
            title="Every account together. Each account one click away."
            description="Work from a single unified inbox, then switch to Personal, Work, or University whenever context matters. No browser profiles. No tab archaeology."
            bullets={[
              "Unified inbox and search",
              "Separate account and folder views",
              "Works across different providers",
            ]}
            scene={<UnifiedScene />}
          />
          <FeatureStory
            reversed
            title="Gmail-like categorization, but for all email accounts."
            description="Dakia’s bundled local model sorts incoming mail into easy to glance categories, and remembers when you correct it."
            bullets={[
              "Runs entirely on your device",
              "No mailbox profiling in the cloud",
            ]}
            scene={<CategoryScene />}
          />
          <FeatureStory
            title={offlineTranslationStory.title}
            description={offlineTranslationStory.description}
            bullets={[...offlineTranslationStory.bullets]}
            scene={<TranslationScene />}
          />
          <div className="compact-feature-heading">
            <p className="eyebrow">The everyday details</p>
            <h3>Fast where email usually slows you down.</h3>
          </div>
          <div className="compact-feature-grid">
            <CompactFeature
              featured
              title="New mail arrives without constant polling."
              description="When a provider supports IMAP IDLE, Dakia listens for mailbox changes and refreshes promptly."
              bullets={[
                "Native macOS notifications",
                "Polling fallback when needed",
              ]}
              scene={<IdleNotificationScene />}
            />
            <CompactFeature
              title="Find the whole story."
              description="Search every account, open the conversation, and keep its attachments close."
              bullets={["Unified local search", "Threaded results"]}
              scene={<SearchScene />}
            />
            <CompactFeature
              title="Unsubscribe without the footer hunt."
              description="When a sender supports the standard, Dakia puts the action beside the message."
              bullets={["Standards-based", "Works across supported accounts"]}
              scene={<UnsubscribeScene />}
            />
          </div>
        </div>
      </section>

      <section className="privacy-section">
        <div className="shell privacy-layout">
          <Reveal className="privacy-copy">
            <p className="eyebrow">Local means local</p>
            <h2>Your inbox is not our business model.</h2>
            <p>
              Credentials, messages, search, and categorization stay on your
              computer. Dakia connects directly to your email providers and
              collects no telemetry.
            </p>
            <div className="privacy-points">
              <span>
                <IconCommand size={17} /> Runs on your computer
              </span>
              <span>
                <IconSearch size={17} /> Local search and categorization
              </span>
              <span>
                <IconLanguage size={17} /> Offline email translation
              </span>
              <span>
                <IconLock size={17} /> No Dakia cloud mailbox
              </span>
            </div>
            <Link to="/privacy" className="text-link">
              Read the privacy policy <IconArrowRight size={16} />
            </Link>
          </Reveal>
          <Reveal>
            <PrivacyScene />
          </Reveal>
        </div>
      </section>

      <section className="switch-section">
        <div className="shell switch-grid">
          <Reveal className="switch-copy">
            <p className="eyebrow">For people leaving Spark</p>
            <h2>
              Keep the smart inbox.
              <br />
              Keep your mail local.
            </h2>
            <p>
              A fast, cross-platform smart inbox should not require sending mail
              through another company’s cloud or accepting a monthly upgrade
              pitch.
            </p>
            <p>
              Dakia keeps the individual email experience people appreciate
              while connecting directly to providers and remaining free to use.
            </p>
          </Reveal>
          <Reveal className="comparison-table">
            <div className="comparison-head">
              <span />
              <b>Dakia</b>
              <b>Spark</b>
            </div>
            {[
              ["Core price", "Free", "Free tier + paid plans"],
              ["App-cloud email processing", "None", "For some features"],
              ["Automatic categorization", "Yes - with corrections", "Yes"],
              ["Team collaboration", "No", "Yes"],
              [
                'Constant Reminders to "Upgrade"',
                "No",
                "Yes and getting worse",
              ],
            ].map((row, index) => (
              <div className="comparison-row" key={row[0]}>
                <span>{row[0]}</span>
                {reduce ? (
                  <b>{row[1]}</b>
                ) : (
                  <motion.b
                    initial={{ opacity: 0, scale: 0.88 }}
                    whileInView={{ opacity: 1, scale: 1 }}
                    viewport={{ once: true, amount: 0.6 }}
                    transition={{
                      delay: 0.35 + index * 0.12,
                      duration: 0.4,
                      ease: [0.16, 1, 0.3, 1],
                    }}
                  >
                    {row[1]}
                  </motion.b>
                )}
                <span>{row[2]}</span>
              </div>
            ))}
          </Reveal>
        </div>
      </section>

      <section className="cli-section">
        <div className="shell cli-layout">
          <Reveal className="cli-copy">
            <p className="eyebrow">Your inbox, from the command line</p>
            <h2>Give your terminal and your agents a way into every inbox.</h2>
            <p>
              Dakia’s CLI works with the accounts and mail catalogue already on
              your computer. Search across inboxes, refresh mail, inspect a
              message, save its attachments, or archive it without opening the
              app.
            </p>
            <div className="cli-usecases">
              <span>
                <IconSearch size={17} /> Search unread mail across every account
              </span>
              <span>
                <IconSparkles size={17} /> Refresh the local mail catalogue
              </span>
              <span>
                <IconCommand size={17} /> Read a message from the terminal
              </span>
              <span>
                <IconFile size={17} /> Download an attachment to disk
              </span>
              <span>
                <IconTerminal2 size={17} /> Archive handled messages
              </span>
            </div>
            <p className="coming-note">
              CLI ships with the public macOS release.
            </p>
          </Reveal>
          <Reveal>
            <CliScene />
          </Reveal>
        </div>
      </section>

      <section className="roadmap-section">
        <div className="shell">
          <Reveal className="roadmap-heading">
            <p className="eyebrow">Built to keep moving</p>
            <h2>Useful today. Extensible tomorrow.</h2>
          </Reveal>
          <div className="roadmap-grid">
            <Reveal className="roadmap-item">
              <span>
                <IconGitBranch size={21} />
              </span>
              <small>Coming soon</small>
              <h3>Windows & Linux</h3>
              <p>
                Dakia is cross-platform by design. macOS launches first, with
                Windows and Linux next.
              </p>
            </Reveal>
          </div>
        </div>
      </section>

      <section className="final-cta">
        <div className="shell final-cta-inner">
          <div className="final-cta-copy">
            <p className="eyebrow">Free and open source for macOS</p>
            <h2>
              Put every inbox
              <br />
              in its place.
            </h2>
            <p>
              Every core feature is free. Support is optional. Available for
              Apple Silicon and Intel.
            </p>
          </div>
          <div className="final-cta-actions">
            <DownloadButton />
            <Link to="/pricing" className="text-link text-link-light">
              See pricing details <IconArrowRight size={16} />
            </Link>
          </div>
        </div>
      </section>
    </main>
  );
}
