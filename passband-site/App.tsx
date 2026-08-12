import { useEffect, useMemo, useRef } from "react";

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
  card: {
    display: "flex",
    flexDirection: "column",
    alignItems: "center",
    gap: "1rem",
    padding: "1.25rem 1.5rem 1.5rem",
    borderRadius: "1rem",
    background: "rgba(255, 255, 255, 0.055)",
    border: "1px solid rgba(255, 255, 255, 0.11)",
  },
  cardLabel: {
    fontSize: "0.72rem",
    fontWeight: 600,
    letterSpacing: "0.12em",
    textTransform: "uppercase",
    color: "#8a8a90",
  },
  tile: {
    display: "flex",
    flexDirection: "column",
    alignItems: "center",
    gap: "0.6rem",
    padding: "1rem 1.25rem",
    borderRadius: "0.75rem",
    minWidth: "7.5rem",
    color: "#d6d6d9",
    textDecoration: "none",
    fontSize: "0.9rem",
    background: "rgba(255, 255, 255, 0.04)",
    border: "1px solid rgba(255, 255, 255, 0.09)",
  },
  pill: {
    padding: "0.28rem 0.8rem",
    borderRadius: "999px",
    border: "1px solid #3a3a3f",
    color: "#b8b8bd",
    fontSize: "0.75rem",
    background: "transparent",
    fontFamily: "inherit",
  },
  button: {
    padding: "0.55rem 1.4rem",
    borderRadius: "0.6rem",
    background: "#f5f5f7",
    color: "#111",
    textDecoration: "none",
    fontSize: "0.9rem",
    fontWeight: 500,
  },
  corner: {
    position: "absolute",
    bottom: "1.25rem",
    zIndex: 2,
    display: "flex",
    gap: "1.25rem",
  },
  link: {
    color: "#8a8a90",
    fontSize: "0.9rem",
    textDecoration: "none",
  },
} as const;

const iconProps = {
  width: 30,
  height: 30,
  viewBox: "0 0 24 24",
  fill: "none",
  stroke: "currentColor",
  strokeWidth: 1.8,
  strokeLinecap: "round",
  strokeLinejoin: "round",
} as const;

const TerminalIcon = () => (
  <svg {...iconProps} aria-hidden="true">
    <polyline points="4 17 10 11 4 5" />
    <line x1="12" y1="19" x2="20" y2="19" />
  </svg>
);

const CloudIcon = () => (
  <svg {...iconProps} aria-hidden="true">
    <path d="M18 10h-1.26A8 8 0 1 0 9 20h9a5 5 0 0 0 0-10z" />
  </svg>
);

const DownloadIcon = () => (
  <svg {...iconProps} width={38} height={38} aria-hidden="true">
    <path d="M21 15v4a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2v-4" />
    <polyline points="7 10 12 15 17 10" />
    <line x1="12" y1="15" x2="12" y2="3" />
  </svg>
);

function FakeInbox() {
  // One randomized batch of rows, rendered twice so the scroll can wrap
  // seamlessly: when the offset passes one copy's height it resets mod that
  // height and the second copy is pixel-identical to where the first began.
  const rows = useMemo(
    () =>
      Array.from({ length: 60 }, () => {
        const [sender, subject, snippet] =
          FAKE_EMAILS[Math.floor(Math.random() * FAKE_EMAILS.length)];
        const h = Math.floor(Math.random() * 12) + 1;
        const m = String(Math.floor(Math.random() * 60)).padStart(2, "0");
        const ampm = Math.random() < 0.5 ? "AM" : "PM";
        return { sender, subject, snippet, time: `${h}:${m} ${ampm}` };
      }),
    [],
  );

  const scrollRef = useRef<HTMLDivElement>(null);

  // Fake doomscroll: a flick of random distance and speed, a random pause,
  // repeat forever. rAF drives each flick; timeouts space them out.
  useEffect(() => {
    if (matchMedia("(prefers-reduced-motion: reduce)").matches) return;
    const el = scrollRef.current;
    if (!el) return;
    let offset = 0;
    let raf = 0;
    let timer: ReturnType<typeof setTimeout>;
    let cancelled = false;
    const easeOut = (t: number) => 1 - Math.pow(1 - t, 3);

    const flick = () => {
      if (cancelled) return;
      const distance = 40 + Math.random() * 280;
      const duration = 800 + Math.random() * 1400;
      const from = offset;
      const start = performance.now();
      const frame = (now: number) => {
        if (cancelled) return;
        const t = Math.min(1, (now - start) / duration);
        offset = from + distance * easeOut(t);
        const wrap = el.scrollHeight / 2 || 1;
        el.style.transform = `translateY(-${offset % wrap}px)`;
        if (t < 1) raf = requestAnimationFrame(frame);
        else timer = setTimeout(flick, 150 + Math.random() * 1100);
      };
      raf = requestAnimationFrame(frame);
    };

    timer = setTimeout(flick, 600);
    return () => {
      cancelled = true;
      cancelAnimationFrame(raf);
      clearTimeout(timer);
    };
  }, []);

  return (
    <div style={styles.inbox} aria-hidden="true">
      <div ref={scrollRef} style={{ willChange: "transform" }}>
        {[...rows, ...rows].map(({ sender, subject, snippet, time }, i) => (
          <div key={i} style={styles.row}>
            <span style={styles.sender}>{sender}</span>
            <span style={styles.subject}>{subject}</span>
            <span style={styles.snippet}>{snippet}</span>
            <span style={styles.time}>{time}</span>
          </div>
        ))}
      </div>
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
        <p style={styles.tagline}>fuck email. lets make it bearable</p>
        <div
          style={{
            display: "flex",
            flexWrap: "wrap",
            justifyContent: "center",
            alignItems: "stretch",
            gap: "1rem",
            marginTop: "0.75rem",
            padding: "0 1.5rem",
          }}
        >
          <section style={styles.card}>
            <span style={styles.cardLabel}>the daemon</span>
            <div style={{ display: "flex", gap: "0.75rem" }}>
              <a style={styles.tile} href="/self-host">
                <TerminalIcon />
                self-host
              </a>
              <div style={styles.tile}>
                <CloudIcon />
                hosted
                {/* Waitlist wiring comes later; this is a visual placeholder. */}
                <button style={styles.pill} type="button">
                  join waitlist
                </button>
              </div>
            </div>
          </section>
          <section style={styles.card}>
            <span style={styles.cardLabel}>the mac app</span>
            <div
              style={{
                flex: 1,
                display: "flex",
                flexDirection: "column",
                alignItems: "center",
                justifyContent: "center",
                gap: "1rem",
                color: "#d6d6d9",
                padding: "0 1rem",
              }}
            >
              <DownloadIcon />
              <a style={styles.button} href="/download/latest">
                download
              </a>
            </div>
          </section>
        </div>
      </div>
      <footer style={{ ...styles.corner, left: "1.5rem" }}>
        <a style={styles.link} href="https://github.com/braelyn-ai/squelch">
          GitHub
        </a>
      </footer>
      <footer style={{ ...styles.corner, right: "1.5rem" }}>
        <a style={styles.link} href="/privacy">
          Privacy
        </a>
        <a style={styles.link} href="/terms">
          Terms
        </a>
      </footer>
    </main>
  );
}
