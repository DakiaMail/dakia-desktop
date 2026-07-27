import { useEffect, useRef, useState } from "react";
import { m as motion, useReducedMotion } from "motion/react";
import {
  IconArchive,
  IconBell,
  IconBolt,
  IconCheck,
  IconChevronRight,
  IconCommand,
  IconFile,
  IconMail,
  IconLanguage,
  IconLock,
  IconPencil,
  IconRefresh,
  IconSearch,
  IconSend,
  IconStar,
  IconTrash,
  IconUsers,
  IconX,
} from "@tabler/icons-react";

const mails = [
  {
    sender: "Mina Park",
    subject: "Your welcome kit is ready",
    preview: "A small welcome kit is ready for your first week.",
    account: "Paperkite Studio",
    time: "04:36",
    tone: "coral",
    starred: true,
  },
  {
    sender: "Harbor Receipts",
    subject: "Monthly spending snapshot",
    preview: "Here is your clean monthly snapshot.",
    account: "Harbor",
    time: "Jul 24",
    tone: "blue",
    starred: true,
  },
  {
    sender: "Leah Ortiz",
    subject: "Planning next week's workshop",
    preview: "I reserved the west room and added a simple run of show.",
    account: "Northstar",
    time: "Jul 24",
    tone: "green",
    starred: false,
  },
  {
    sender: "Alder & Pine",
    subject: "Receipt · Alder & Pine",
    preview: "Thanks for your order. Your desk tray will ship tomorrow.",
    account: "Paperkite Studio",
    time: "00:36",
    tone: "amber",
    starred: false,
  },
];

const accounts = [
  { label: "All inboxes", count: 12, tone: "all" },
  { label: "Paperkite Studio", count: 4, tone: "coral" },
  { label: "Northstar", count: 3, tone: "green" },
  { label: "Harbor", count: 5, tone: "blue" },
];

export const translationSceneCopy = {
  original: {
    subject: "Reedese töötoa plaan",
    greeting: "Tere, Alex!",
    body: "Töötuba algab reedel kell kümme. Lisasin lõpliku plaani ja ruumide kaardi.",
    attachment: "Töötoa plaan",
  },
  translated: {
    subject: "Friday’s workshop plan",
    greeting: "Hi Alex,",
    body: "The workshop starts at ten on Friday. I attached the final plan and room map.",
    attachment: "Workshop plan",
  },
  action: "Translate to English",
  restoreAction: "Show original",
  progress: "Translating privately on this device…",
  status: "Translated from Estonian on this device",
} as const;

function Frame({
  children,
  className = "",
}: {
  children: React.ReactNode;
  className?: string;
}) {
  return <div className={`demo-frame ${className}`}>{children}</div>;
}

function DemoTopbar({ label = "Inbox" }: { label?: string }) {
  return (
    <div className="demo-topbar">
      <div className="traffic" aria-hidden="true">
        <i />
        <i />
        <i />
      </div>
      <span>{label}</span>
      <IconSearch size={15} stroke={1.8} />
    </div>
  );
}

function SceneToolbar({
  title,
  mode = "smart",
}: {
  title: string;
  mode?: "smart" | "list";
}) {
  return (
    <div className="scene-toolbar">
      <div className="traffic" aria-hidden="true">
        <i />
        <i />
        <i />
      </div>
      <b>{title}</b>
      <div className="scene-view-toggle" aria-label={`${mode} view selected`}>
        <span className={mode === "smart" ? "active" : ""}>Smart</span>
        <span className={mode === "list" ? "active" : ""}>List</span>
      </div>
      <span className="scene-compose">
        <IconPencil size={12} /> Compose
      </span>
      <IconRefresh className="scene-refresh" size={15} />
    </div>
  );
}

export function HeroControlCenter() {
  const reduce = useReducedMotion();
  return (
    <motion.div
      className="hero-product"
      initial={reduce ? false : { opacity: 0, y: 24, rotate: 0.8 }}
      animate={{ opacity: 1, y: 0, rotate: 0 }}
      transition={{ duration: 0.8, ease: [0.16, 1, 0.3, 1], delay: 0.12 }}
    >
      <div className="hero-glow" aria-hidden="true" />
      <Frame className="hero-frame">
        <div className="mail-app">
          <aside className="mail-sidebar">
            <div className="sidebar-traffic traffic" aria-hidden="true">
              <i />
              <i />
              <i />
            </div>
            <p>Mail</p>
            <div className="account-row active folder-root">
              <IconMail size={14} />
              <span>Inbox</span>
              <b>⌄</b>
            </div>
            <div className="account-nest">
              {accounts.slice(1).map((account, index) => (
                <motion.div
                  className="account-row"
                  key={account.label}
                  initial={reduce ? false : { opacity: 0, x: -10 }}
                  animate={{ opacity: 1, x: 0 }}
                  transition={{ delay: 0.35 + index * 0.07 }}
                >
                  <i className={`account-dot ${account.tone}`} />
                  <span>{account.label}</span>
                </motion.div>
              ))}
            </div>
            <p className="sidebar-label">Folders</p>
            <div className="folder-list">
              <div className="account-row">
                <IconMail size={14} />
                <span>All mail</span>
              </div>
              <div className="account-row">
                <IconBolt size={14} />
                <span>Unread</span>
              </div>
              <div className="account-row">
                <IconStar size={14} />
                <span>Starred</span>
                <b>3</b>
              </div>
              <div className="account-row">
                <IconSend size={14} />
                <span>Sent</span>
              </div>
              <div className="account-row">
                <IconPencil size={14} />
                <span>Drafts</span>
              </div>
              <div className="account-row">
                <IconArchive size={14} />
                <span>Archive</span>
              </div>
              <div className="account-row">
                <IconTrash size={14} />
                <span>Trash</span>
              </div>
            </div>
          </aside>
          <section className="mail-list">
            <div className="list-heading">
              <div className="list-title-row">
                <b>Inbox</b>
                <div className="view-toggle">
                  <span>Smart</span>
                  <span>List</span>
                </div>
                <button type="button">
                  <IconPencil size={12} /> Compose
                </button>
                <IconRefresh size={15} />
              </div>
              <label className="list-search">
                <IconSearch size={13} />
                <span>Search every account…</span>
              </label>
            </div>
            <p className="mail-group-label">Starred</p>
            {mails.map((mail, index) => (
              <motion.div
                className={`mail-row ${index === 0 ? "selected" : ""}`}
                key={mail.subject}
                initial={reduce ? false : { opacity: 0, y: 12 }}
                animate={{ opacity: 1, y: 0 }}
                transition={{ delay: 0.45 + index * 0.08, duration: 0.42 }}
              >
                <i className="mail-select" />
                <div>
                  <strong>{mail.sender}</strong>
                  <span>{mail.subject}</span>
                  <small>{mail.preview}</small>
                </div>
                <div className="mail-meta">
                  <time>{mail.time}</time>
                  <IconStar
                    className={mail.starred ? "is-starred" : ""}
                    size={13}
                  />
                </div>
              </motion.div>
            ))}
          </section>
          <section className="mail-reader">
            <div className="reader-actions">
              <IconArchive size={15} />
              <IconMail size={15} />
              <IconTrash size={15} />
              <IconStar className="reader-star" size={16} />
            </div>
            <div className="reader-title">Your welcome kit is ready</div>
            <div className="reader-person">
              <i className="avatar coral">M</i>
              <div>
                <b>Mina Park</b>
                <span>mina@paperkite.test · To alex@paperkite.test</span>
              </div>
              <time>Saturday 25 Jul at 04:36</time>
            </div>
            <p>
              Hi Alex Morgan,
              <br />
              <br />
              A small welcome kit is ready for your first week. The studio guide
              and neighborhood map are attached.
              <br />
              <br />
              Best,
              <br />
              Mina Park
            </p>
          </section>
        </div>
      </Frame>
    </motion.div>
  );
}

export function UnifiedScene() {
  const [mergeCount, setMergeCount] = useState(0);
  const reduce = useReducedMotion();

  useEffect(() => {
    if (reduce) {
      setMergeCount(3);
      return;
    }
    const timer = window.setTimeout(
      () => setMergeCount((stage) => (stage === 3 ? 0 : stage + 1)),
      mergeCount === 3 ? 1800 : 1250,
    );
    return () => window.clearTimeout(timer);
  }, [mergeCount, reduce]);

  const inboxes = [
    { name: "Paperkite", tone: "coral", count: 4 },
    { name: "Northstar", tone: "green", count: 3 },
    { name: "Harbor", tone: "blue", count: 5 },
  ];
  const unifiedRows = [
    "Welcome kit",
    "Weekly plan",
    "Receipt #1048",
    "Workshop notes",
    "Travel update",
    "Project handoff",
    "Neighbourhood guide",
    "Monthly snapshot",
    "The Sunday Edit",
  ];

  return (
    <div
      className={`unified-merge-demo merge-stage-${mergeCount}`}
      aria-label="Three separate inboxes merge into one growing unified inbox"
    >
      <div className="merge-canvas">
        {inboxes.map((inbox, index) => (
          <div
            className={`source-inbox source-${index} ${mergeCount > index ? "merged" : ""}`}
            key={inbox.name}
          >
            <span className={`source-dot ${inbox.tone}`} />
            <b>{inbox.name}</b>
            <small>{inbox.count} messages</small>
            <i />
            <i />
            <i />
          </div>
        ))}
        <div className={`unified-inbox merge-stage-${mergeCount}`}>
          <div className="unified-inbox-head">
            <span>Unified inbox</span>
            <b>{[0, 4, 7, 12][mergeCount]}</b>
          </div>
          <div className="unified-account-line">
            <span className="source-dot coral" />
            <span className="source-dot green" />
            <span className="source-dot blue" />
            <small>
              {mergeCount === 0
                ? "Waiting for accounts"
                : "All accounts together"}
            </small>
          </div>
          <div className="unified-list">
            {unifiedRows.slice(0, mergeCount * 3).map((subject, index) => (
              <div className="unified-message" key={subject}>
                <i
                  className={`source-dot ${["coral", "green", "blue"][index % 3]}`}
                />
                <span>{subject}</span>
                <small />
              </div>
            ))}
            {mergeCount === 0 && (
              <div className="unified-empty">Ready to bring it together</div>
            )}
          </div>
        </div>
      </div>
    </div>
  );
}

const categories = [
  { name: "People", tone: "coral", icon: IconUsers, example: "Maya Chen" },
  {
    name: "Transactions",
    tone: "amber",
    icon: IconFile,
    example: "Northwind receipt",
  },
  {
    name: "Notifications",
    tone: "blue",
    icon: IconBolt,
    example: "Campus Library",
  },
  {
    name: "Newsletters",
    tone: "green",
    icon: IconMail,
    example: "The Sunday Edit",
  },
];

const categoryJobs = [
  {
    sender: "Maya Chen",
    subject: "Dinner on Friday?",
    preview: "I found a table near the studio.",
    category: "People",
  },
  {
    sender: "Northwind",
    subject: "Receipt #1048",
    preview: "Your payment receipt is ready.",
    category: "Transactions",
  },
  {
    sender: "Campus Library",
    subject: "Book due reminder",
    preview: "The book is due back this Friday.",
    category: "Notifications",
  },
  {
    sender: "The Sunday Edit",
    subject: "Issue 118",
    preview: "Four small ways to make next week calmer.",
    category: "Newsletters",
  },
] as const;

const categoryDurations = [1100, 1000, 900, 1800] as const;

function useSceneTick(stageCount: number, durations?: readonly number[]) {
  const reduce = useReducedMotion();
  const [tick, setTick] = useState(() => (reduce ? stageCount - 1 : 0));
  const stage = tick % stageCount;
  const duration =
    durations?.[stage] ?? (stage === stageCount - 1 ? 2000 : 1300);

  useEffect(() => {
    if (reduce) {
      setTick(stageCount - 1);
      return;
    }
    const timer = window.setTimeout(() => setTick(tick + 1), duration);
    return () => window.clearTimeout(timer);
  }, [duration, reduce, stageCount, tick]);

  return [tick, setTick, reduce] as const;
}

export function CategoryScene() {
  const [tick, , reduce] = useSceneTick(4, categoryDurations);
  const stage = tick % 4;
  const jobIndex = Math.floor(tick / 4) % categoryJobs.length;
  const job = categoryJobs[jobIndex];
  const [ridingIn, setRidingIn] = useState(false);

  useEffect(() => {
    setRidingIn(false);
    if (reduce || stage !== 0) return;
    const timer = window.setTimeout(() => setRidingIn(true), 20);
    return () => window.clearTimeout(timer);
  }, [reduce, stage]);

  return (
    <div
      className={`category-flow-demo category-stage-${stage} category-job-${jobIndex}`}
      aria-label="An email arrives at the Dakia brain, is processed, and fans out into a category bucket"
    >
      <div className="category-canvas">
        <div className="category-brain" aria-hidden="true">
          <span>
            <img src="/dakia-icon.png" alt="" />
          </span>
          <small>Dakia brain</small>
        </div>
        <div className="category-bins">
          {categories.map((category, index) => {
            const Icon = category.icon;
            const isTarget = category.name === job.category;
            const isFiled = reduce || (stage === 3 && isTarget);
            const count =
              [4, 7, 3, 2][index] +
              (reduce || (stage === 3 && isTarget) ? 1 : 0);
            return (
              <div
                className={`category-bin ${category.tone} ${isFiled ? "filed" : ""}`}
                key={category.name}
              >
                <span className="category-bin-icon">
                  <Icon size={15} />
                </span>
                <b>{category.name}</b>
                <strong>{count}</strong>
              </div>
            );
          })}
        </div>
        {!reduce && (
          <div
            className={`category-traveler ${ridingIn ? "riding-in" : ""}`}
            key={job.sender}
          >
            <i className="mail-select" />
            <div>
              <b>{job.sender}</b>
              <span>{job.subject}</span>
              <small>{job.preview}</small>
            </div>
            {stage === 1 && <i className="category-scan" aria-hidden="true" />}
          </div>
        )}
      </div>
    </div>
  );
}

export function TranslationScene() {
  const [tick, setTick, reduce] = useSceneTick(3, [900, 1500, 2400]);
  const stage = reduce ? 2 : tick % 3;
  const translated = stage === 2;
  return (
    <Frame
      className={`feature-demo translation-demo translation-stage-${stage}`}
    >
      <div className="translation-demo-toolbar">
        <div className="traffic" aria-hidden="true">
          <i />
          <i />
          <i />
        </div>
        <div className="translation-demo-actions">
          <IconArchive size={15} />
          <IconMail size={15} />
          <IconTrash size={15} />
        </div>
        <button
          type="button"
          onClick={() => setTick((current) => current - (current % 3) + 2)}
        >
          <IconLanguage size={14} />
          {translated
            ? translationSceneCopy.restoreAction
            : translationSceneCopy.action}
        </button>
      </div>
      <div className="translation-demo-reader">
        <motion.h4
          key={translated ? "subject-en" : "subject-et"}
          initial={reduce ? false : { opacity: 0, y: 4 }}
          animate={{ opacity: 1, y: 0 }}
        >
          {translationSceneCopy[translated ? "translated" : "original"].subject}
        </motion.h4>
        <div className="reader-person">
          <i className="avatar green">K</i>
          <div>
            <b>Kadri Tamm</b>
            <span>kadri@northstar.test · To Alex Morgan</span>
          </div>
        </div>
        {stage === 1 ? (
          <div className="translation-demo-progress">
            <IconLanguage size={14} />
            {translationSceneCopy.progress}
          </div>
        ) : null}
        {translated ? (
          <div className="translation-demo-status">
            <IconCheck size={14} /> {translationSceneCopy.status}
          </div>
        ) : null}
        <motion.div
          className="translation-demo-message"
          key={translated ? "message-en" : "message-et"}
          initial={reduce ? false : { opacity: 0 }}
          animate={{ opacity: 1 }}
        >
          <p>
            {
              translationSceneCopy[translated ? "translated" : "original"]
                .greeting
            }
          </p>
          <p>
            {translationSceneCopy[translated ? "translated" : "original"].body}
          </p>
          <div className="translation-demo-card">
            <IconFile size={16} />
            <span>
              <b>
                {
                  translationSceneCopy[translated ? "translated" : "original"]
                    .attachment
                }
              </b>
              <small>PDF · 1.2 MB</small>
            </span>
          </div>
          <p>{translated ? "Best,\nKadri" : "Parimat,\nKadri"}</p>
        </motion.div>
        <small className="translation-demo-offline">
          <IconLock size={13} /> Language pack installed · works offline
        </small>
      </div>
    </Frame>
  );
}

const searchMessages = [
  {
    subject: "Re: launch checklist",
    sender: "Jonas Meyer",
    snippet: "Everything for Friday is now in one thread.",
    time: "09:42",
    matches: 3,
  },
  {
    subject: "Launch day travel",
    sender: "Mina Park",
    snippet: "Your train details are in the shared plan.",
    time: "Jul 24",
    matches: 2,
  },
  {
    subject: "Research lab launch",
    sender: "Campus Lab",
    snippet: "The launch brief is ready for review.",
    time: "Jul 21",
    matches: 4,
  },
  {
    subject: "Receipt #1048",
    sender: "Northwind",
    snippet: "Your payment receipt is ready.",
    time: "Jul 20",
    matches: 1,
  },
  {
    subject: "Receipt · Alder & Pine",
    sender: "Alder & Pine",
    snippet: "Thanks for your order.",
    time: "Jul 19",
    matches: 1,
  },
  {
    subject: "Planning next week's workshop",
    sender: "Leah Ortiz",
    snippet: "I reserved the west room.",
    time: "Jul 18",
    matches: 2,
  },
  {
    subject: "Workshop notes",
    sender: "Maya Chen",
    snippet: "Notes and next steps from the workshop.",
    time: "Jul 17",
    matches: 2,
  },
] as const;

const searchQueries = ["launch", "receipt", "workshop"] as const;

export function SearchScene() {
  const reduce = useReducedMotion();
  const [query, setQuery] = useState(() => (reduce ? "launch" : ""));
  const [queryIndex, setQueryIndex] = useState(0);
  const [typedLength, setTypedLength] = useState(0);
  const [phase, setPhase] = useState<"typing" | "holding" | "clearing">(
    "typing",
  );
  const [paused, setPaused] = useState(false);
  const resumeTimer = useRef<number | undefined>(undefined);
  const targetQuery = searchQueries[queryIndex];

  useEffect(() => {
    if (reduce) {
      setQuery("launch");
      return;
    }
    if (paused) return;

    if (phase === "typing") {
      if (typedLength < targetQuery.length) {
        const timer = window.setTimeout(() => {
          const nextLength = typedLength + 1;
          setQuery(targetQuery.slice(0, nextLength));
          setTypedLength(nextLength);
        }, 105);
        return () => window.clearTimeout(timer);
      }
      const timer = window.setTimeout(() => setPhase("holding"), 1600);
      return () => window.clearTimeout(timer);
    }
    if (phase === "holding") {
      const timer = window.setTimeout(() => setPhase("clearing"), 40);
      return () => window.clearTimeout(timer);
    }
    if (typedLength > 0) {
      const timer = window.setTimeout(() => {
        const nextLength = typedLength - 1;
        setQuery(targetQuery.slice(0, nextLength));
        setTypedLength(nextLength);
      }, 40);
      return () => window.clearTimeout(timer);
    }
    setQueryIndex((current) => (current + 1) % searchQueries.length);
    setPhase("typing");
  }, [paused, phase, reduce, targetQuery, typedLength]);

  useEffect(() => () => window.clearTimeout(resumeTimer.current), []);

  const pauseSearch = () => {
    window.clearTimeout(resumeTimer.current);
    setPaused(true);
  };
  const resumeSearch = () => {
    window.clearTimeout(resumeTimer.current);
    resumeTimer.current = window.setTimeout(() => {
      setTypedLength(0);
      setPhase("typing");
      setPaused(false);
    }, 4000);
  };
  const filteredMessages = searchMessages.filter(
    (message) =>
      message.subject.toLowerCase().includes(query.toLowerCase()) ||
      query === "",
  );
  const activeResult = filteredMessages[0] ?? searchMessages[0];
  return (
    <Frame className="feature-demo search-demo">
      <SceneToolbar title="Inbox" mode="list" />
      <div className="search-scene-body">
        <div className="search-list-pane">
          <label className="demo-search">
            <IconSearch size={16} />
            <input
              value={query}
              onChange={(event) => {
                pauseSearch();
                setQuery(event.target.value);
              }}
              onFocus={pauseSearch}
              onBlur={resumeSearch}
              aria-label="Try the search animation"
            />
            <IconX size={13} />
          </label>
          <p className="search-meta">
            {filteredMessages.length} results across every account · searched
            locally
          </p>
          <div className="search-results">
            {filteredMessages.map((message) => (
              <motion.div
                layout
                className="search-result"
                key={message.subject}
              >
                <i className="mail-select" />
                <div>
                  <b>{message.subject}</b>
                  <span>{message.sender}</span>
                  <small>{message.matches} messages in thread</small>
                </div>
                <time>{message.time}</time>
              </motion.div>
            ))}
          </div>
        </div>
        <article className="search-reader">
          <div className="reader-actions">
            <IconArchive size={15} />
            <IconMail size={15} />
            <IconTrash size={15} />
            <IconStar className="reader-star" size={16} />
          </div>
          <h4>{activeResult.subject}</h4>
          <div className="reader-person">
            <i className="avatar green">{activeResult.sender.slice(0, 1)}</i>
            <div>
              <b>{activeResult.sender}</b>
              <span>mail@dakia.test · To Alex Morgan</span>
            </div>
          </div>
          <p>{activeResult.snippet}</p>
          {query && (
            <motion.div
              className="search-hit"
              key={`${query}-${filteredMessages.length}`}
              initial={reduce ? false : { opacity: 0, y: 4 }}
              animate={{ opacity: 1, y: 0 }}
            >
              <IconSearch size={13} /> “{query}” found in{" "}
              {filteredMessages.length} messages
            </motion.div>
          )}
        </article>
      </div>
    </Frame>
  );
}

export function UnsubscribeScene() {
  const [tick, setTick, reduce] = useSceneTick(4);
  const stage = tick % 4;
  const done = reduce || stage >= 2;
  const confirmed = reduce || stage >= 3;
  return (
    <Frame
      className={`feature-demo unsubscribe-demo unsubscribe-stage-${stage}`}
    >
      <div className="unsubscribe-toolbar">
        <div className="traffic" aria-hidden="true">
          <i />
          <i />
          <i />
        </div>
        <div className="unsubscribe-actions">
          <IconArchive size={15} />
          <IconMail size={15} />
          <IconTrash size={15} />
          <IconStar size={16} />
        </div>
      </div>
      <div className="unsubscribe-reader">
        <h4>Small rituals for a better week</h4>
        <div className="newsletter-head">
          <span className="newsletter-mark">S</span>
          <div>
            <b>The Sunday Edit</b>
            <small>news@fictional.example · To alex@paperkite.test</small>
            <button
              className={done ? "unsubscribe-chip done" : "unsubscribe-chip"}
              onClick={() => setTick((current) => current - (current % 4) + 2)}
              type="button"
            >
              {done ? (
                <>
                  <IconCheck size={12} /> Unsubscribed
                </>
              ) : (
                "Unsubscribe"
              )}
            </button>
          </div>
          <time>Sunday at 08:15</time>
        </div>
        {confirmed && (
          <p className="unsubscribe-confirmation">
            You'll stop receiving The Sunday Edit. This email stays in your
            inbox.
          </p>
        )}
        <div className="newsletter-content">
          <p>The Sunday Edit</p>
          <strong>Four small ways to make next week calmer.</strong>
          <span>Issue 118 · a five-minute read</span>
          <div className="newsletter-art" aria-hidden="true">
            <i />
            <i />
            <i />
          </div>
        </div>
      </div>
    </Frame>
  );
}

export function IdleNotificationScene() {
  const [tick, , reduce] = useSceneTick(4);
  const stage = tick % 4;
  return (
    <div
      className={`idle-flow-demo idle-stage-${stage}`}
      aria-label="New mail appears and a native notification follows"
    >
      <div className="idle-flow-canvas">
        <div className="idle-mailbox">
          <div className="idle-mailbox-head">
            <div>
              <b>Inbox</b>
              <span>Paperkite Studio</span>
            </div>
            <span className="idle-status">
              <i />
            </span>
          </div>
          <div className="idle-row existing">
            <i className="mail-select" />
            <div>
              <b>Leah Ortiz</b>
              <span>Planning next week’s workshop</span>
              <small>Jul 24</small>
            </div>
          </div>
          {(reduce || stage >= 1) && (
            <div
              className={`idle-incoming-slot ${stage === 3 && !reduce ? "collapsing" : ""}`}
            >
              <div className="idle-incoming-clip">
                <div
                  className="idle-row incoming"
                  key={stage === 1 ? tick : "incoming"}
                >
                  <i className="mail-select" />
                  <div>
                    <b>Mina Park</b>
                    <span>Your welcome kit is ready</span>
                    <small>Now</small>
                  </div>
                  <IconStar className="is-starred" size={14} />
                </div>
              </div>
            </div>
          )}
          <div className="idle-row existing">
            <i className="mail-select" />
            <div>
              <b>Harbor Receipts</b>
              <span>Monthly spending snapshot</span>
              <small>Jul 24</small>
            </div>
          </div>
        </div>
        {(reduce || stage >= 2) && (
          <aside
            className="native-notice"
            aria-label="Example macOS notification"
          >
            <div className="native-notice-mark">
              <img src="/dakia-icon.png" alt="" />
            </div>
            <div>
              <small>DAKIA · NOW</small>
              <b>Mina Park</b>
              <span>Your welcome kit is ready</span>
            </div>
            <IconBell size={15} />
          </aside>
        )}
      </div>
    </div>
  );
}

const providerLogos = [
  {
    name: "Gmail",
    path: "M24 5.457v13.909c0 .904-.732 1.636-1.636 1.636h-3.819V11.73L12 16.64l-6.545-4.91v9.273H1.636A1.636 1.636 0 0 1 0 19.366V5.457c0-2.023 2.309-3.178 3.927-1.964L5.455 4.64 12 9.548l6.545-4.91 1.528-1.145C21.69 2.28 24 3.434 24 5.457z",
  },
  {
    name: "Microsoft Outlook",
    path: "M7.88 12.04q0 .45-.11.87-.1.41-.33.74-.22.33-.58.52-.37.2-.87.2t-.85-.2q-.35-.21-.57-.55-.22-.33-.33-.75-.1-.42-.1-.86t.1-.87q.1-.43.34-.76.22-.34.59-.54.36-.2.87-.2t.86.2q.35.21.57.55.22.34.31.77.1.43.1.88zM24 12v9.38q0 .46-.33.8-.33.32-.8.32H7.13q-.46 0-.8-.33-.32-.33-.32-.8V18H1q-.41 0-.7-.3-.3-.29-.3-.7V7q0-.41.3-.7Q.58 6 1 6h6.5V2.55q0-.44.3-.75.3-.3.75-.3h12.9q.44 0 .75.3.3.3.3.75V10.85l1.24.72h.01q.1.07.18.18.07.12.07.25zm-6-8.25v3h3v-3zm0 4.5v3h3v-3zm0 4.5v1.83l3.05-1.83zm-5.25-9v3h3.75v-3zm0 4.5v3h3.75v-3zm0 4.5v2.03l2.41 1.5 1.34-.8v-2.73zM9 3.75V6h2l.13.01.12.04v-2.3zM5.98 15.98q.9 0 1.6-.3.7-.32 1.19-.86.48-.55.73-1.28.25-.74.25-1.61 0-.83-.25-1.55-.24-.71-.71-1.24t-1.15-.83q-.68-.3-1.55-.3-.92 0-1.64.3-.71.3-1.2.85-.5.54-.75 1.3-.25.74-.25 1.63 0 .85.26 1.56.26.72.74 1.23.48.52 1.17.81.69.3 1.56.3zM7.5 21h12.39L12 16.08V17q0 .41-.3.7-.29.3-.7.3H7.5zm15-.13v-7.24l-5.9 3.54Z",
  },
  {
    name: "Fastmail",
    path: "M1.954 18.554A11.943 11.943 0 0 1 0 12C0 5.377 5.383 0 12.006 0c4.117 0 7.753 2.078 9.915 5.242L19.82 6.643a9.468 9.468 0 0 0-7.814-4.118c-5.229 0-9.475 4.246-9.475 9.475 0 1.9.56 3.669 1.524 5.153zM22.06 5.45A11.938 11.938 0 0 1 24 12c0 6.623-5.371 12-11.994 12a11.994 11.994 0 0 1-9.913-5.238l2.101-1.401a9.47 9.47 0 0 0 7.812 4.114c5.229 0 9.475-4.246 9.475-9.475a9.426 9.426 0 0 0-1.522-5.15zM6.301 15.656V8.198l5.59 3.731zm11.41-7.307v7.052a.398.398 0 0 1-.401.401H6.533z",
  },
] as const;

export function PrivacyScene() {
  const [tick, , reduce] = useSceneTick(4);
  const stage = tick % 4;
  return (
    <div
      className={`privacy-scene privacy-stage-${stage}`}
      aria-label="Email providers connect directly to Dakia on your device"
    >
      <div className="provider-cloud">
        {providerLogos.map((provider) => (
          <svg
            viewBox="0 0 24 24"
            fill="currentColor"
            aria-hidden="true"
            key={provider.name}
          >
            <path d={provider.path} />
          </svg>
        ))}
        <small>Your providers</small>
      </div>
      <div className="data-lane">
        {[0, 1, 2].map((index) => (
          <i className={reduce ? "arrived" : ""} key={index} />
        ))}
        <span>TLS</span>
      </div>
      <div className={`local-device ${reduce || stage >= 2 ? "arrived" : ""}`}>
        <div className="device-laptop">
          <div className="device-screen">
            <img src="/dakia-icon.png" alt="" />
          </div>
        </div>
      </div>
    </div>
  );
}

const commands = [
  {
    label: "Search",
    command: 'dakia --json search "invoice" --unread --limit 20',
    result: "Unread invoice matches, ready for scripts",
  },
  {
    label: "Sync",
    command: "dakia sync --limit 250",
    result: "Mail refreshed from every enabled account",
  },
  {
    label: "Read",
    command: "dakia show MESSAGE_ID",
    result: "Full message fetched from the provider",
  },
  {
    label: "Download Attachment",
    command:
      "dakia attachment download MESSAGE_ID ATTACHMENT_ID --output ./invoice.pdf",
    result: "Attachment saved without overwriting an existing file",
  },
  {
    label: "Archive",
    command: "dakia archive MESSAGE_ID",
    result: "Message moved to the provider’s archive",
  },
];

export function CliScene() {
  const reduce = useReducedMotion();
  const [active, setActive] = useState(0);
  const [typedLength, setTypedLength] = useState(() =>
    reduce ? commands[0].command.length : 0,
  );
  const [paused, setPaused] = useState(false);
  const resumeTimer = useRef<number | undefined>(undefined);
  const command = commands[active].command;
  const complete = reduce || typedLength === command.length;

  useEffect(() => {
    if (reduce) {
      setTypedLength(command.length);
      return;
    }
    if (paused) return;
    if (typedLength < command.length) {
      const timer = window.setTimeout(
        () => setTypedLength((length) => length + 1),
        42,
      );
      return () => window.clearTimeout(timer);
    }
    const timer = window.setTimeout(() => {
      setActive((current) => (current + 1) % commands.length);
      setTypedLength(0);
    }, 1400);
    return () => window.clearTimeout(timer);
  }, [command.length, paused, reduce, typedLength]);

  useEffect(() => () => window.clearTimeout(resumeTimer.current), []);

  const selectCommand = (index: number) => {
    window.clearTimeout(resumeTimer.current);
    setPaused(true);
    setActive(index);
    setTypedLength(commands[index].command.length);
    resumeTimer.current = window.setTimeout(() => {
      setPaused(false);
      setActive((current) => (current + 1) % commands.length);
      setTypedLength(0);
    }, 5000);
  };
  return (
    <Frame className="cli-scene">
      <DemoTopbar label="Terminal · dakia" />
      <div className="terminal-body">
        <p>
          <span>~</span> {command.slice(0, typedLength)}
          <i />
        </p>
        {complete && (
          <motion.div
            key={active}
            initial={reduce ? false : { opacity: 0, y: 6 }}
            animate={{ opacity: 1, y: 0 }}
          >
            <IconCheck size={15} /> {commands[active].result}
          </motion.div>
        )}
      </div>
      <div className="terminal-tabs">
        {commands.map(({ label }, index) => (
          <button
            type="button"
            className={active === index ? "active" : ""}
            onClick={() => selectCommand(index)}
            key={label}
          >
            {label}
          </button>
        ))}
      </div>
    </Frame>
  );
}
