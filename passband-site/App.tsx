// The background is a procedurally repeated fake inbox: the drudgery Passband
// exists to kill, blurred into wallpaper so the pitch sits on top of it.
// Snippets are written long so rows run the full viewport width.
const FAKE_EMAILS: Array<[sender: string, subject: string, snippet: string]> = [
  ["LinkedIn", "You appeared in 9 searches this week", "See who's looking for someone like you. Recruiters from companies you've never heard of are searching for profiles matching yours..."],
  ["Medium Daily Digest", "Stories for you", "10 Habits of Highly Effective Engineers | 12 min read. Why I Quit My Job to Farm Mushrooms | 8 min read. The Death of the..."],
  ["no-reply@accounts", "Security alert for your account", "A new sign-in was detected on a device you may or may not recognize. If this was you, no action is needed. If this wasn't..."],
  ["DoorDash", "Your Friday deserves 40% off", "Hungry? Use code FRIYAY at checkout before midnight and get 40% off orders over $35, up to a maximum discount of $8..."],
  ["Confluence", "Weekly digest: 47 updates in spaces you follow", "Q3 Planning Doc was edited by 6 people. Retro Notes (DRAFT) (COPY) was moved. A page you commented on in 2024 was..."],
  ["Marriott Bonvoy", "Points update: you have 312 points", "Your points balance summary for the month. You are 87,688 points away from your next free night at participating..."],
  ["Product Hunt Daily", "The 10 best new products today", "An AI notetaker for your AI notetaker, a smart water bottle that syncs to your calendar, and 8 more launches you'll..."],
  ["billing@saas.io", "Your receipt from Acme SaaS #48211", "Amount paid: $12.00. Thank you for your continued subscription to a service you forgot you signed up for. Manage your..."],
  ["United Airlines", "MileagePlus: miles expiring soon", "Don't lose your 1,204 miles. Book by the end of the month or transfer them to a partner at an exchange rate that will..."],
  ["HR Announcements", "REMINDER: Open enrollment closes Friday", "This is your final reminder to complete your benefits elections. If you do not act, your current elections will roll..."],
  ["Substack", "3 new posts from writers you follow", "The Case Against Breakfast, and other essays. Plus: a 4,000-word post about someone's move to Lisbon and what it..."],
  ["Twitter", "You have 4 new notifications", "@someguy and 3 others liked a post you were mentioned in. See what you're missing. Your network has been busy while..."],
  ["Zoom", "Your cloud recording is now available", "Meeting recording: Weekly Sync. Duration: 58 minutes. This recording will be automatically deleted in 30 days and..."],
  ["Chase", "Your statement is ready", "Your February statement for account ending in 4482 is now available. Your minimum payment is due in 21 days. Log in..."],
  ["GitHub", "[repo] 23 new notifications", "dependabot opened 14 pull requests in repositories you have not touched since 2023. Bump lodash from 4.17.20 to..."],
  ["Eventbrite", "Events near you this weekend", "Networking mixers, fun runs, and a pottery class. Based on your interests: 12 events happening within 25 miles of..."],
  ["Google Calendar", "Daily agenda for Tuesday", "You have 7 events scheduled today starting with Standup at 9:00 AM, followed by a meeting that could have been an..."],
  ["Sephora", "We miss you! Here's 15% off", "It's been a while. Your Beauty Insider points are waiting, and so is 15% off your next purchase of $50 or more..."],
  ["The Team", "We've updated our Privacy Policy", "We're writing to let you know about some updates to our Privacy Policy and Terms of Service, effective in 30 days..."],
  ["Slack", "You have unread messages in #general", "While you were away, 312 messages were posted in channels you follow, including a heated thread about the office..."],
];

const styles = {
  page: {
    position: "relative",
    minHeight: "100vh",
    margin: 0,
    fontFamily: "system-ui, sans-serif",
    overflow: "hidden",
    background: "#0f0f10",
  },
  inbox: {
    position: "absolute",
    // Oversized so the blur doesn't leave washed-out edges at the viewport.
    inset: "-1.5rem",
    background: "#0f0f10",
    overflow: "hidden",
    zIndex: 0,
    filter: "blur(4px)",
  },
  row: {
    display: "flex",
    alignItems: "center",
    gap: "1rem",
    padding: "0.65rem 1.25rem",
    borderBottom: "1px solid #1f1f22",
    whiteSpace: "nowrap",
  },
  sender: {
    width: "13rem",
    flexShrink: 0,
    fontWeight: 600,
    fontSize: "0.85rem",
    color: "#d6d6d9",
    overflow: "hidden",
    textOverflow: "ellipsis",
  },
  subject: {
    fontSize: "0.85rem",
    color: "#c2c2c6",
    overflow: "hidden",
    textOverflow: "ellipsis",
    flexShrink: 0,
  },
  snippet: {
    fontSize: "0.85rem",
    color: "#6b6b70",
    overflow: "hidden",
    textOverflow: "ellipsis",
    flex: 1,
  },
  time: {
    fontSize: "0.75rem",
    color: "#6b6b70",
    flexShrink: 0,
  },
  frost: {
    position: "absolute",
    inset: 0,
    background: "rgba(10, 10, 12, 0.62)",
    zIndex: 1,
  },
  content: {
    position: "relative",
    zIndex: 2,
    minHeight: "100vh",
    display: "flex",
    flexDirection: "column",
    alignItems: "center",
    justifyContent: "center",
    gap: "1rem",
  },
  title: {
    fontSize: "3rem",
    fontWeight: 600,
    margin: 0,
    color: "#f5f5f7",
  },
  tagline: {
    margin: 0,
    color: "#b8b8bd",
    fontSize: "1.1rem",
    textAlign: "center",
    padding: "0 1.5rem",
  },
  link: {
    color: "#8a8a90",
    fontSize: "1rem",
  },
} as const;

function FakeInbox() {
  // Enough rows to cover any viewport; cycle the list so it never runs out.
  const rows = Array.from({ length: 60 }, (_, i) => FAKE_EMAILS[i % FAKE_EMAILS.length]);
  return (
    <div style={styles.inbox} aria-hidden="true">
      {rows.map(([sender, subject, snippet], i) => (
        <div key={i} style={styles.row}>
          <span style={styles.sender}>{sender}</span>
          <span style={styles.subject}>{subject}</span>
          <span style={styles.snippet}>{snippet}</span>
          <span style={styles.time}>{`${(i * 7) % 12 || 12}:${String((i * 13) % 60).padStart(2, "0")} ${i % 2 ? "AM" : "PM"}`}</span>
        </div>
      ))}
    </div>
  );
}

export function App() {
  return (
    <main style={styles.page}>
      <FakeInbox />
      <div style={styles.frost} />
      <div style={styles.content}>
        <img
          src="/knob.png"
          alt="Passband"
          width={112}
          height={112}
          style={{ borderRadius: "1.5rem" }}
        />
        <h1 style={styles.title}>Passband</h1>
        <p style={styles.tagline}>fuck email. its time to make it bearable</p>
        <a
          style={{
            padding: "0.6rem 1.4rem",
            borderRadius: "0.6rem",
            background: "#f5f5f7",
            color: "#111",
            textDecoration: "none",
            fontSize: "0.95rem",
            fontWeight: 500,
          }}
          href="/download/latest"
        >
          Download for Mac
        </a>
        <nav style={{ display: "flex", gap: "1.25rem" }}>
          <a style={styles.link} href="https://github.com/braelyn-ai/squelch">
            GitHub
          </a>
          <a style={styles.link} href="/privacy">
            Privacy
          </a>
          <a style={styles.link} href="/terms">
            Terms
          </a>
        </nav>
      </div>
    </main>
  );
}
