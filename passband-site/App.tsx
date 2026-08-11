// The background is a procedurally repeated fake inbox: the drudgery Passband
// exists to kill, blurred into wallpaper so the pitch sits on top of it.
const FAKE_EMAILS: Array<[sender: string, subject: string, snippet: string]> = [
  ["LinkedIn", "You appeared in 9 searches this week", "See who's looking for someone like you..."],
  ["Medium Daily Digest", "Stories for you", "10 Habits of Highly Effective Engineers | 12 min read..."],
  ["no-reply@accounts", "Security alert for your account", "A new sign-in was detected on a device you..."],
  ["DoorDash", "Your Friday deserves 40% off", "Hungry? Use code FRIYAY at checkout before..."],
  ["Confluence", "Weekly digest: 47 updates in spaces you follow", "Q3 Planning Doc was edited by 6 people..."],
  ["Marriott Bonvoy", "Points update: you have 312 points", "Your points balance summary for the month of..."],
  ["Product Hunt Daily", "The 10 best new products today", "An AI notetaker for your AI notetaker and more..."],
  ["billing@saas.io", "Your receipt from Acme SaaS #48211", "Amount paid: $12.00. Thank you for your continued..."],
  ["United Airlines", "MileagePlus: miles expiring soon", "Don't lose your 1,204 miles. Book by..."],
  ["HR Announcements", "REMINDER: Open enrollment closes Friday", "This is your final reminder to complete your..."],
  ["Substack", "3 new posts from writers you follow", "The Case Against Breakfast, and other essays..."],
  ["Twitter", "You have 4 new notifications", "@someguy and 3 others liked a post you were..."],
  ["Zoom", "Your cloud recording is now available", "Meeting recording: Weekly Sync (no one watched..."],
  ["Chase", "Your statement is ready", "Your February statement for account ending in..."],
  ["GitHub", "[repo] 23 new notifications", "dependabot opened 14 pull requests in repositories..."],
  ["Eventbrite", "Events near you this weekend", "Networking mixers, fun runs, and a pottery class..."],
  ["Google Calendar", "Daily agenda for Tuesday", "You have 7 events scheduled today starting with..."],
  ["Sephora", "We miss you! Here's 15% off", "It's been a while. Your Beauty Insider points are..."],
  ["The Team", "We've updated our Privacy Policy", "We're writing to let you know about some updates..."],
  ["Slack", "You have unread messages in #general", "While you were away, 312 messages were posted in..."],
];

const styles = {
  page: {
    position: "relative",
    minHeight: "100vh",
    margin: 0,
    fontFamily: "system-ui, sans-serif",
    overflow: "hidden",
  },
  inbox: {
    position: "absolute",
    // Oversized so the blur doesn't leave washed-out edges at the viewport.
    inset: "-1.5rem",
    background: "#fff",
    overflow: "hidden",
    zIndex: 0,
    filter: "blur(4px)",
  },
  row: {
    display: "flex",
    alignItems: "center",
    gap: "1rem",
    padding: "0.65rem 1.25rem",
    borderBottom: "1px solid #eee",
    whiteSpace: "nowrap",
  },
  sender: {
    width: "13rem",
    flexShrink: 0,
    fontWeight: 600,
    fontSize: "0.85rem",
    color: "#333",
    overflow: "hidden",
    textOverflow: "ellipsis",
  },
  subject: {
    fontSize: "0.85rem",
    color: "#333",
    overflow: "hidden",
    textOverflow: "ellipsis",
  },
  snippet: {
    fontSize: "0.85rem",
    color: "#999",
    overflow: "hidden",
    textOverflow: "ellipsis",
    flex: 1,
  },
  time: {
    fontSize: "0.75rem",
    color: "#999",
    flexShrink: 0,
  },
  frost: {
    position: "absolute",
    inset: 0,
    background: "rgba(255, 255, 255, 0.65)",
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
  },
  tagline: {
    margin: 0,
    color: "#444",
    fontSize: "1.1rem",
    textAlign: "center",
    padding: "0 1.5rem",
  },
  link: {
    color: "#555",
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
            background: "#1a1a1a",
            color: "#fff",
            textDecoration: "none",
            fontSize: "0.95rem",
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
