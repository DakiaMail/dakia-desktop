export type Post = {
  slug: string;
  title: string;
  description: string;
  date: string;
  readTime: string;
  category: string;
  body: Array<{
    heading?: string;
    text: string;
    sources?: Array<{ label: string; url: string }>;
  }>;
};

// Add a post here to publish it. The blog index and individual article route are generated from this list.
export const posts: Post[] = [
  {
    slug: "dakia-vs-spark",
    title: "Dakia and Spark: an honest comparison",
    description:
      "What Dakia keeps from modern email clients, what it changes, and where Spark still has the advantage.",
    date: "2026-07-21",
    readTime: "6 min read",
    category: "Comparison",
    body: [
      {
        text: "Spark proved that a desktop email client could be fast, polished, and genuinely helpful. Dakia is being built for people who value that experience but do not want their mailbox stored on another company's servers or another recurring subscription in their budget.",
      },
      {
        heading: "Where Dakia takes a different path",
        text: "Dakia is free to use. Credentials, downloaded mail, search, automatic categorization, and email translation stay on your computer. Downloaded translation packs keep working offline. Dakia connects directly to your providers instead of operating a separate cloud mailbox.",
      },
      {
        heading: "The individual comes first",
        text: "Dakia focuses on the person juggling work, university, personal, and project accounts. Its unified inbox, separate account views, threaded conversations, fast search, notifications, attachments, shortcuts, and Gmail-style one-click unsubscribe are designed around that daily reality.",
      },
      {
        heading: "Where Spark remains the better fit",
        text: "Spark has mature team collaboration features. Dakia is not trying to replace shared drafts, internal comments, or other team workflows in its first releases. Teams that rely on those tools should keep that advantage in mind.",
      },
      {
        heading: "A note on comparisons",
        text: "Products and plans change. This comparison describes Dakia's intended public release and Spark's publicly documented product model as of July 2026. We will correct material inaccuracies when we find them.",
        sources: [
          {
            label: "Spark pricing and plan comparison",
            url: "https://sparkmailapp.com/pricing",
          },
          {
            label: "Spark email privacy and server processing",
            url: "https://sparkmailapp.com/help/privacy-data/spark-email-privacy-everything-you-need-to-know",
          },
          {
            label: "Spark shared drafts documentation",
            url: "https://sparkmailapp.com/help/spark-for-teams/shared-drafts-spark",
          },
        ],
      },
    ],
  },
  {
    slug: "a-calmer-home-for-email",
    title: "A calmer home for email",
    description:
      "Why Dakia is designed around attention, ownership, and the quiet parts of a good mail client.",
    date: "2026-07-21",
    readTime: "3 min read",
    category: "Product",
    body: [
      {
        text: "Email is where plans, promises, receipts, and personal notes accumulate. Dakia is being built to make that space feel considered again, without turning it into another feed competing for your attention.",
      },
      {
        heading: "Useful, not noisy",
        text: "The app groups related messages, keeps a clear view of the inbox, and makes the actions you need easy to reach. The goal is less time arranging mail and more time deciding what deserves a reply.",
      },
      {
        heading: "Your mail stays yours",
        text: "Dakia connects directly to the providers you choose. Mail, credentials, search, categorization, and translation are handled locally on your device.",
      },
    ],
  },
  {
    slug: "three-ways-to-put-email-on-autopilot",
    title: "Three ways to put email on autopilot",
    description:
      "A daily brief, inbox-zero triage, and recurring workflows built on Dakia's command-line interface.",
    date: "2026-07-21",
    readTime: "4 min read",
    category: "Automation",
    body: [
      {
        text: "A desktop app is useful when you are at your desk. A command-line interface makes the same mail system useful inside scripts, scheduled jobs, and personal automation workflows.",
      },
      {
        heading: "Start the day with a priority brief",
        text: "Search unread mail across every account and assemble a concise priority list before you open the inbox. The source mail remains in the local Dakia data store.",
      },
      {
        heading: "Triage toward inbox zero",
        text: "Use structured search results to separate receipts, notifications, and real conversations. Scripts can prepare a plan while leaving irreversible actions for you to approve.",
      },
      {
        heading: "Automate the recurring work",
        text: "Shell scripts can find routine messages and send from the correct account. The important part is not one clever demo—it is having a composable interface for the workflows only you know you need.",
      },
    ],
  },
];
