import { useEffect, useState, type ReactNode } from "react";
import {
  IconArrowRight,
  IconBrandApple,
  IconBrandGithub,
  IconChevronDown,
  IconMenu2,
  IconMoon,
  IconSun,
  IconX,
} from "@tabler/icons-react";
import { Home } from "./home";

export const site = {
  githubUrl: "https://github.com/DakiaMail/dakia-desktop",
  redditUrl: "https://www.reddit.com/r/Dakia/",
  supportEmail: "support@dakiamail.com",
  legalEmail: "support@dakiamail.com",
  macDownloads: {
    appleSilicon:
      "https://downloads.dakiamail.com/macos/latest/Dakia-Apple-Silicon.dmg",
    intel: "https://downloads.dakiamail.com/macos/latest/Dakia-Intel.dmg",
  },
};

export const headerNavigation = [
  ["Features", "/#features"],
  ["Privacy", "/privacy"],
  ["Pricing", "/pricing"],
  ["Security", "/security"],
  ["Support", "/support"],
  ["About", "/about"],
  ["GitHub", site.githubUrl],
] as const;

export const footerCompanyLinks = [
  ["About", "/about"],
  ["GitHub", site.githubUrl],
  ["Reddit", site.redditUrl],
] as const;

type Theme = "light" | "dark";

function useTheme() {
  const [theme, setTheme] = useState<Theme>(() => {
    const saved = window.localStorage.getItem("dakia-theme") as Theme | null;
    return (
      saved ??
      (window.matchMedia("(prefers-color-scheme: dark)").matches
        ? "dark"
        : "light")
    );
  });
  useEffect(() => {
    document.documentElement.dataset.theme = theme;
    window.localStorage.setItem("dakia-theme", theme);
  }, [theme]);
  return { theme, setTheme };
}

function navigate(to: string) {
  const [path, hash] = to.split("#");
  window.history.pushState({}, "", to);
  window.dispatchEvent(new PopStateEvent("popstate"));
  if (hash)
    window.requestAnimationFrame(() =>
      document.getElementById(hash)?.scrollIntoView({ behavior: "smooth" }),
    );
  else if (path) window.scrollTo({ top: 0, behavior: "auto" });
}

function useLocationPath() {
  const [location, setLocation] = useState(
    `${window.location.pathname}${window.location.hash}`,
  );
  useEffect(() => {
    const update = () =>
      setLocation(`${window.location.pathname}${window.location.hash}`);
    window.addEventListener("popstate", update);
    window.addEventListener("hashchange", update);
    return () => {
      window.removeEventListener("popstate", update);
      window.removeEventListener("hashchange", update);
    };
  }, []);
  return location;
}

export function AppLink({
  to,
  children,
  className,
  "aria-current": ariaCurrent,
  "aria-label": ariaLabel,
}: {
  to: string;
  children: ReactNode;
  className?: string;
  "aria-current"?: "page";
  "aria-label"?: string;
}) {
  const external = /^https?:/.test(to);
  return (
    <a
      className={className}
      href={to}
      aria-current={ariaCurrent}
      aria-label={ariaLabel}
      onClick={(event) => {
        if (!external && to.startsWith("/")) {
          event.preventDefault();
          navigate(to);
        }
      }}
    >
      {children}
    </a>
  );
}

function Brand() {
  return (
    <AppLink to="/" className="brand" aria-label="Dakia home">
      <img src="/dakia-icon.png" alt="" />
      <span>Dakia</span>
    </AppLink>
  );
}

export function DownloadButton({ compact = false }: { compact?: boolean }) {
  return (
    <details className="download-menu">
      <summary
        className={`button button-primary download-trigger ${
          compact ? "button-compact" : ""
        }`}
      >
        <IconBrandApple size={18} />
        <span className="download-full-label">Download for macOS</span>
        {compact ? <span className="download-short-label">Mac</span> : null}
        <IconChevronDown className="download-chevron" size={16} />
      </summary>
      <div className="download-options">
        <a
          href={site.macDownloads.appleSilicon}
          aria-label="Download Dakia for Apple Silicon Macs"
        >
          <span>
            <strong>Apple Silicon</strong>
            <small>M1, M2, M3, M4, and newer</small>
          </span>
          <small className="download-recommended">Recommended</small>
        </a>
        <a
          href={site.macDownloads.intel}
          aria-label="Download Dakia for Intel Macs"
        >
          <span>
            <strong>Intel</strong>
            <small>Intel-based Macs</small>
          </span>
        </a>
        <a
          className="download-help"
          href="https://support.apple.com/en-us/116943"
          target="_blank"
          rel="noreferrer"
        >
          Not sure which Mac you have?
        </a>
      </div>
    </details>
  );
}

function Header({
  theme,
  setTheme,
}: {
  theme: Theme;
  setTheme: (theme: Theme) => void;
}) {
  const [open, setOpen] = useState(false);
  const location = useLocationPath();
  const toggleTheme = () => setTheme(theme === "light" ? "dark" : "light");
  useEffect(() => setOpen(false), [location]);
  useEffect(() => {
    if (!open) return;
    const previous = document.body.style.overflow;
    document.body.style.overflow = "hidden";
    return () => {
      document.body.style.overflow = previous;
    };
  }, [open]);
  return (
    <>
      <header className="site-header">
        <div className="shell-wide nav-shell">
          <Brand />
          <nav
            className={open ? "nav-links open" : "nav-links"}
            aria-label="Main navigation"
          >
            {headerNavigation.map(([label, path]) => {
              const active = path.startsWith("https:")
                ? false
                : path === "/#features"
                  ? location === "/#features"
                  : location.split("#", 1)[0] === path;
              return (
                <AppLink
                  to={path}
                  className={active ? "active" : undefined}
                  aria-current={active ? "page" : undefined}
                  key={path}
                >
                  {label === "GitHub" ? (
                    <>
                      <IconBrandGithub size={16} /> {label}
                    </>
                  ) : (
                    label
                  )}
                </AppLink>
              );
            })}
          </nav>
          <div className="nav-actions">
            <button
              className="icon-button"
              type="button"
              aria-label={`Use ${theme === "light" ? "dark" : "light"} theme`}
              onClick={toggleTheme}
            >
              {theme === "light" ? (
                <IconMoon size={18} />
              ) : (
                <IconSun size={18} />
              )}
            </button>
            <DownloadButton compact />
            <button
              className="icon-button menu-button"
              type="button"
              aria-label={open ? "Close menu" : "Open menu"}
              aria-expanded={open}
              onClick={() => setOpen(!open)}
            >
              {open ? <IconX /> : <IconMenu2 />}
            </button>
          </div>
        </div>
      </header>
      {open ? (
        <button
          className="nav-scrim"
          type="button"
          aria-label="Dismiss navigation"
          onClick={() => setOpen(false)}
        />
      ) : null}
    </>
  );
}

function Footer() {
  return (
    <footer className="site-footer">
      <div className="shell footer-grid">
        <div className="footer-lead">
          <Brand />
          <p>
            A free, open-source multi-account email control center that keeps
            your mail local.
          </p>
        </div>
        <FooterGroup
          title="Product"
          links={[
            ["Features", "/#features"],
            ["Pricing", "/pricing"],
            ["Security", "/security"],
            ["Support", "/support"],
          ]}
        />
        <FooterGroup title="Company" links={footerCompanyLinks} />
        <FooterGroup
          title="Legal"
          links={[
            ["Privacy", "/privacy"],
            ["Terms", "/terms"],
          ]}
        />
      </div>
      <div className="shell footer-bottom">
        <span>© {new Date().getFullYear()} Dakia</span>
      </div>
    </footer>
  );
}

function FooterGroup({
  title,
  links,
}: {
  title: string;
  links: ReadonlyArray<readonly [string, string]>;
}) {
  return (
    <div className="footer-group">
      <h2>{title}</h2>
      {links.map(([label, path]) => (
        <AppLink to={path} key={path}>
          {label}
        </AppLink>
      ))}
    </div>
  );
}

const standardPages: Record<
  string,
  {
    kicker: string;
    title: string;
    intro: string;
    highlights: Array<[string, string]>;
    action?: { label: string; href: string };
    sections: Array<[string, ReactNode]>;
  }
> = {
  "/about": {
    kicker: "About Dakia",
    title: "A postman for every part of your digital life.",
    intro:
      "Dakia takes its name from the Urdu word for postman. It is an open-source mail client for people whose email life no longer fits neatly into one account—or one browser tab.",
    highlights: [
      ["Focus", "One personal command center"],
      ["Privacy", "Mail stays on your computer"],
      ["Source", "Open on GitHub"],
    ],
    action: { label: "View source on GitHub", href: site.githubUrl },
    sections: [
      [
        "Why Dakia exists",
        <p key="why">
          Modern email clients often ask people to choose between good design,
          privacy, and a fair price. Dakia is an attempt to remove that
          trade-off: a fast, thoughtful desktop client that keeps your mail
          local, is free to use, and develops in public.
        </p>,
      ],
      [
        "Who it is for",
        <p key="who">
          Dakia is for individuals juggling university, work, personal, and
          independent-project inboxes. It is not being built as a team
          collaboration suite. The focus is a superb personal command center for
          email.
        </p>,
      ],
      [
        "Built in the open",
        <p key="open-source">
          The source code, issue tracker, and contribution guide are public on{" "}
          <a href={site.githubUrl}>GitHub</a>. You can inspect the
          implementation, report a bug, propose an improvement, or build Dakia
          yourself.
        </p>,
      ],
      [
        "Where it is going",
        <p key="where">
          The macOS release comes first, for Apple Silicon and Intel. Windows
          and Linux are coming soon, followed by more offline translation
          languages and deeper automation through the CLI.
        </p>,
      ],
    ],
  },
  "/security": {
    kicker: "Security",
    title: "A smaller trust boundary by design.",
    intro:
      "Dakia connects your computer directly to the providers you choose. It does not operate a separate cloud mailbox service.",
    highlights: [
      ["Connections", "Direct to your providers"],
      ["Credentials", "Encrypted local vault"],
      ["Telemetry", "None"],
    ],
    sections: [
      [
        "Local credential storage",
        <p key="local">
          Passwords and OAuth refresh tokens are stored in Dakia’s encrypted
          local credential vault. Protecting your operating-system account
          remains important.
        </p>,
      ],
      [
        "Encrypted connections",
        <p key="connections">
          Dakia uses encrypted provider connections where configured and
          supports standard IMAP and SMTP providers. Provider authorization
          methods depend on the account being connected.
        </p>,
      ],
      [
        "Offline translation",
        <p key="translation">
          Translation uses downloadable language packs and runs entirely on your
          device. Email content is not sent to Dakia or a translation provider,
          and installed language packs continue working offline.
        </p>,
      ],
      [
        "Responsible disclosure",
        <p key="disclosure">
          Security reports can be sent to{" "}
          <a href={`mailto:${site.legalEmail}`}>{site.legalEmail}</a>. Do not
          include passwords, live authorization codes, or sensitive mailbox
          contents.
        </p>,
      ],
    ],
  },
  "/support": {
    kicker: "Support",
    title: "Help when email gets complicated.",
    intro:
      "Get help installing Dakia, connecting an account, or understanding how a feature works.",
    highlights: [
      ["Accounts", "Major providers and IMAP"],
      ["Cost", "Free for everyone"],
      ["Please omit", "Passwords and mailbox content"],
    ],
    action: {
      label: "Email support",
      href: `mailto:${site.supportEmail}`,
    },
    sections: [
      [
        "What to include",
        <p key="include">
          Include your macOS version, Dakia version, provider name, and a short
          description of what happened. Never send a password, OAuth code, or
          sensitive message content.
        </p>,
      ],
      [
        "Supported accounts",
        <p key="accounts">
          Dakia supports Gmail, Outlook.com, Fastmail, Zoho, Migadu, iCloud
          Mail, Yahoo Mail, and custom IMAP/SMTP accounts.
        </p>,
      ],
      [
        "Contact",
        <p key="contact">
          Email <a href={`mailto:${site.supportEmail}`}>{site.supportEmail}</a>.
        </p>,
      ],
      [
        "Response times",
        <p key="response-times">
          Support is provided on a best-effort basis for everyone. Contributing
          to Dakia’s development is completely optional and does not affect the
          support you receive.
        </p>,
      ],
    ],
  },
};

function StandardPage({ page }: { page: (typeof standardPages)[string] }) {
  return (
    <main className="standard-page">
      <div className="shell narrow-shell">
        <header className="page-heading">
          <p className="eyebrow">{page.kicker}</p>
          <h1>{page.title}</h1>
          <p>{page.intro}</p>
          {page.action ? (
            <a className="button button-primary" href={page.action.href}>
              {page.action.label} <IconArrowRight size={17} />
            </a>
          ) : null}
        </header>
        <dl className="page-highlights">
          {page.highlights.map(([label, value]) => (
            <div key={label}>
              <dt>{label}</dt>
              <dd>{value}</dd>
            </div>
          ))}
        </dl>
        <article className="prose">
          {page.sections.map(([heading, content]) => (
            <section key={heading}>
              <h2>{heading}</h2>
              {content}
            </section>
          ))}
        </article>
      </div>
    </main>
  );
}

function Pricing() {
  return (
    <main className="standard-page">
      <div className="shell">
        <header className="page-heading narrow-heading">
          <p className="eyebrow">Pricing</p>
          <h1>Free means free.</h1>
          <p>
            Dakia is free to use and does not require a subscription. Every core
            feature belongs in the app, not behind an upgrade nag.
          </p>
        </header>
        <section className="pricing-plan">
          <div>
            <p className="eyebrow">Dakia for individuals</p>
            <h2>$0</h2>
            <span>Free to use</span>
          </div>
          <ul>
            <li>Unlimited supported email accounts</li>
            <li>Unified inbox and search</li>
            <li>Automatic local categorization</li>
            <li>Threads, attachments, and notifications</li>
            <li>One-click unsubscribe</li>
            <li>Completely optional support for development</li>
          </ul>
          <DownloadButton />
        </section>
      </div>
    </main>
  );
}

function LegalPage({ kind }: { kind: "privacy" | "terms" }) {
  const privacy = kind === "privacy";
  const sections: Array<[string, ReactNode]> = privacy
    ? [
        [
          "What this policy covers",
          <p key="scope">
            This policy explains how the Dakia desktop application and website
            handle information. Dakia is provided by Mashal Tech OÜ (Estonian
            registry code 17198029). Questions can be sent to{" "}
            <a href={`mailto:${site.legalEmail}`}>{site.legalEmail}</a>.
          </p>,
        ],
        [
          "The Dakia desktop application",
          <p key="app">
            Dakia stores account settings, credentials, downloaded mail, search
            indexes, categories, and preferences locally on your device. Dakia
            does not operate a cloud mailbox or collect telemetry about how you
            use the application.
          </p>,
        ],
        [
          "Email providers",
          <p key="providers">
            The application connects directly to providers you choose. Providers
            process your account and message information under their own privacy
            policies. Removing an account removes Dakia’s local account data and
            credentials; it does not delete mail held by your provider.
          </p>,
        ],
        [
          "Offline translation",
          <p key="translation">
            Dakia can download language packs for on-device translation. Email
            subjects and message bodies are processed locally and are not sent
            to Dakia or a third-party translation service. Installed language
            packs can be removed in Settings.
          </p>,
        ],
        [
          "Optional command-line AI processing",
          <p key="ai">
            If you deliberately use Dakia's optional AI command, the selected
            email content is sent to the provider and endpoint you configure.
            Dakia does not operate that provider or receive a copy of the
            request.
          </p>,
        ],
        [
          "Website data",
          <p key="web">
            The website is hosted on Cloudflare Pages. Cloudflare may process
            standard request data such as IP address, browser type, requested
            page, and timestamp to deliver and secure the website. We use
            Cloudflare Web Analytics to understand aggregated website traffic
            and page performance. Cloudflare states that its Web Analytics does
            not use cookies or local storage for these metrics and does not
            track visitors across customers&apos; websites. We do not
            intentionally keep website-request logs; any processing and
            retention performed automatically by Cloudflare, including Web
            Analytics data, is subject to Cloudflare&apos;s own policies.
          </p>,
        ],
        [
          "Your choices and rights",
          <p key="rights">
            You can remove local accounts and data through the application.
            Depending on where you live, you may also have rights to access,
            correct, delete, restrict, or object to personal-data processing. To
            make a request or ask a privacy question, email{" "}
            <a href={`mailto:${site.legalEmail}`}>{site.legalEmail}</a>. We may
            ask for information reasonably necessary to verify your identity
            before responding to a request.
          </p>,
        ],
        [
          "Changes",
          <p key="changes">
            Updates will be published on this page with a revised effective
            date.
          </p>,
        ],
      ]
    : [
        [
          "Agreement",
          <p key="agreement">
            These Terms govern your use of the Dakia website and desktop
            application, provided by Mashal Tech OÜ (Estonian registry code
            17198029). By using Dakia, you agree to these Terms.
          </p>,
        ],
        [
          "Your accounts",
          <p key="accounts">
            You are responsible for accounts connected to Dakia, compliance with
            provider terms, and protection of your device and operating-system
            account.
          </p>,
        ],
        [
          "Acceptable use",
          <p key="use">
            Do not use Dakia unlawfully, infringe another person’s rights,
            disrupt providers, or attempt to access information you are not
            authorized to access.
          </p>,
        ],
        [
          "Third-party services",
          <p key="third">
            Email providers you choose operate under their own terms. Dakia does
            not control their availability, content, or pricing.
          </p>,
        ],
        [
          "Price and support",
          <p key="price">
            Dakia is provided free of charge. Support for development is
            optional, is a one-time payment, and does not create a recurring
            software subscription or entitlement to features or support. If a
            payment was made by mistake or charged more than once, contact{" "}
            <a href={`mailto:${site.legalEmail}`}>{site.legalEmail}</a>. Other
            refunds are not offered except where required by applicable law.
          </p>,
        ],
        [
          "Availability and updates",
          <p key="updates">
            Dakia may update, modify, or discontinue features and cannot
            guarantee uninterrupted compatibility with every provider, device,
            or message format.
          </p>,
        ],
        [
          "Disclaimers and liability",
          <p key="liability">
            To the extent permitted by law, Dakia is provided &ldquo;as
            is&rdquo; and without warranties that it will be uninterrupted,
            error-free, secure, or compatible with every service. To the extent
            permitted by law, Mashal Tech OÜ is not liable for indirect,
            incidental, special, consequential, or punitive loss arising from
            use of Dakia. Nothing in these Terms excludes or limits rights or
            liability that cannot lawfully be excluded or limited. These Terms
            are governed by Estonian law; however, consumers retain the
            mandatory protections of the law of their country of residence.
            Please contact us first at{" "}
            <a href={`mailto:${site.legalEmail}`}>{site.legalEmail}</a> so we
            can try to resolve a concern. Any dispute will be handled by the
            courts with jurisdiction under applicable law.
          </p>,
        ],
      ];
  return (
    <main className="standard-page legal-page">
      <div className="shell narrow-shell">
        <header className="page-heading">
          <p className="eyebrow">Legal</p>
          <h1>{privacy ? "Privacy Policy" : "Terms and Conditions"}</h1>
          <p>Effective date: July 21, 2026. Last updated: July 24, 2026.</p>
        </header>
        <article className="prose legal-copy">
          {sections.map(([heading, content]) => (
            <section key={heading}>
              <h2>{heading}</h2>
              {content}
            </section>
          ))}
        </article>
      </div>
    </main>
  );
}

function NotFound() {
  return (
    <main className="standard-page">
      <div className="shell narrow-shell page-heading">
        <p className="eyebrow">404</p>
        <h1>This postman took a wrong turn.</h1>
        <p>
          The page may have moved, but your inbox is still where you left it.
        </p>
        <AppLink to="/" className="button button-primary">
          Return home <IconArrowRight size={17} />
        </AppLink>
      </div>
    </main>
  );
}
function PageRouter() {
  const [path, setPath] = useState(window.location.pathname);
  useEffect(() => {
    const update = () => setPath(window.location.pathname);
    window.addEventListener("popstate", update);
    return () => window.removeEventListener("popstate", update);
  }, []);
  useEffect(() => {
    const labels: Record<string, string> = {
      "/": "Every inbox. One command center.",
      "/privacy": "Privacy Policy",
      "/terms": "Terms and Conditions",
      "/pricing": "Pricing",
      "/about": "About",
      "/support": "Support",
      "/security": "Security",
    };
    document.title = `${labels[path] ?? "Dakia"} | Dakia`;
    const description =
      path === "/"
        ? "A free, open-source, local-first multi-account email client for macOS. Your mail and credentials stay on your computer."
        : "Dakia is the free, open-source, local-first multi-account email client for macOS.";
    document
      .querySelector('meta[name="description"]')
      ?.setAttribute("content", description);
    const canonicalUrl = `https://dakiamail.com${path === "/" ? "/" : path}`;
    document
      .querySelector('link[rel="canonical"]')
      ?.setAttribute("href", canonicalUrl);
    document
      .querySelector('meta[property="og:url"]')
      ?.setAttribute("content", canonicalUrl);
  }, [path]);
  if (path === "/")
    return <Home Link={AppLink} DownloadButton={DownloadButton} />;
  if (path === "/pricing") return <Pricing />;
  if (path === "/privacy" || path === "/terms")
    return <LegalPage kind={path.slice(1) as "privacy" | "terms"} />;
  return standardPages[path] ? (
    <StandardPage page={standardPages[path]} />
  ) : (
    <NotFound />
  );
}

export function Site() {
  const { theme, setTheme } = useTheme();
  return (
    <>
      <Header theme={theme} setTheme={setTheme} />
      <PageRouter />
      <Footer />
    </>
  );
}
