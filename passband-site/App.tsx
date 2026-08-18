import {
  useEffect,
  useMemo,
  useRef,
  useState,
  type FormEvent,
  type PointerEvent,
} from "react";

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
  // The brand's one serif moment, rationed exactly as Typo does it in the
  // Swift client: Newsreader for the wordmark and nowhere else. Spread
  // further, a display serif becomes wallpaper. Weight 500 matches
  // Typo.hero's .medium; the font's opsz axis does the rest on its own.
  title: {
    fontFamily: '"Newsreader", ui-serif, Georgia, serif',
    fontSize: "3.4rem",
    fontWeight: 500,
    letterSpacing: "-0.005em",
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
  cardCopy: {
    margin: 0,
    maxWidth: "22rem",
    color: "#b8b8bd",
    fontSize: "0.95rem",
    lineHeight: 1.5,
    textAlign: "center",
  },
  // A COLUMN, not a row. The join button is the same piece of hardware the
  // landing page leads with, and that button is far too tall to stand beside a
  // text field without one of the two looking wrong.
  waitlistForm: {
    display: "flex",
    flexDirection: "column",
    alignSelf: "stretch",
    gap: "0.55rem",
  },
  waitlistInput: {
    padding: "0.55rem 0.8rem",
    borderRadius: "0.6rem",
    border: "1px solid rgba(255, 255, 255, 0.14)",
    background: "rgba(255, 255, 255, 0.04)",
    color: "#f5f5f7",
    fontSize: "0.95rem",
    fontFamily: "inherit",
    width: "100%",
    boxSizing: "border-box",
  },
  status: {
    margin: 0,
    color: "#b8b8bd",
    fontSize: "0.9rem",
    textAlign: "center",
  },
  // The line under the button, for the people the button is not for: anyone
  // holding an invite already, who would otherwise find the homepage a dead
  // end now that the download has moved into the flow behind it.
  note: {
    margin: "0.4rem 0 0",
    color: "#8a8a90",
    fontSize: "0.85rem",
    textAlign: "center",
  },
  noteLink: {
    color: "#b8b8bd",
    textUnderlineOffset: "0.15em",
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

// Brass and steel lifted off the app icon. Nothing else on the page carries a
// hue, so this accent belongs to the button alone and reads as the one lit
// instrument in a dark room.
const BRASS = "240, 204, 128";

// The passband's half-width when the filter is fully open. Everything narrower
// is this times the opening, which is what keeps the hump one shape instead of
// a stretching blob. Module scope because the pointer handler sizes its travel
// limit from it and the draw loop sizes the curve from it.
const FULL_W = 0.19;

const CTA_CSS = `
.pb-cta {
  position: relative;
  isolation: isolate;
  display: inline-flex;
  align-items: center;
  justify-content: center;
  gap: 0.6rem;
  margin-top: 0.65rem;
  padding: 0.9rem 1.9rem 1.3rem;
  border-radius: 0.9rem;
  overflow: hidden;
  text-decoration: none;
  font-family: inherit;
  font-size: 0.95rem;
  font-weight: 600;
  letter-spacing: 0.015em;
  cursor: pointer;
  appearance: none;
  color: #e9e2d4;
  background: linear-gradient(180deg, #1e1e22, #131315);
  border: 1px solid rgba(${BRASS}, 0.22);
  box-shadow:
    inset 0 1px 0 rgba(255, 255, 255, 0.07),
    0 12px 30px rgba(0, 0, 0, 0.5);
  transition:
    transform 0.4s cubic-bezier(0.2, 0.8, 0.2, 1),
    border-color 0.4s ease,
    box-shadow 0.4s ease,
    color 0.4s ease;
}
.pb-cta:hover,
.pb-cta:focus-visible {
  color: #fff6e4;
  transform: translateY(-2px);
  border-color: rgba(${BRASS}, 0.62);
  box-shadow:
    inset 0 1px 0 rgba(255, 255, 255, 0.14),
    0 0 34px -6px rgba(${BRASS}, 0.45),
    0 16px 38px rgba(0, 0, 0, 0.55);
}
.pb-cta:active { transform: translateY(0) scale(0.995); }
.pb-cta:focus-visible {
  outline: 2px solid rgba(${BRASS}, 0.75);
  outline-offset: 3px;
}
/* Machined top edge: a filament that comes up with the rest of the hardware. */
.pb-cta::before {
  content: "";
  position: absolute;
  inset: 0 0 auto;
  height: 1px;
  z-index: 2;
  opacity: 0.45;
  background: linear-gradient(90deg, transparent, rgba(${BRASS}, 0.8), transparent);
  transition: opacity 0.4s ease;
}
.pb-cta:hover::before,
.pb-cta:focus-visible::before { opacity: 1; }
.pb-cta-meter {
  position: absolute;
  inset: 0;
  z-index: 0;
  width: 100%;
  height: 100%;
}
.pb-cta-arrow,
.pb-cta-label { position: relative; z-index: 1; }
.pb-cta-arrow { transition: transform 0.4s cubic-bezier(0.2, 0.8, 0.2, 1); }
.pb-cta:hover .pb-cta-arrow,
.pb-cta:focus-visible .pb-cta-arrow { transform: translateX(3px); }
/* The submit button in the waitlist card spans its field, so the meter under
   the label is the full width of the form rather than a stub in the middle. */
.pb-cta-wide { display: flex; width: 100%; margin-top: 0; }
/* In flight. The meter keeps running (the request is the thing being waited
   on) but the hardware stops answering the pointer. */
.pb-cta[disabled] { cursor: progress; color: #a9a49a; }
.pb-cta[disabled]:hover,
.pb-cta[disabled]:hover .pb-cta-arrow { transform: none; }
.pb-cta[disabled]:hover { border-color: rgba(${BRASS}, 0.22); box-shadow:
  inset 0 1px 0 rgba(255, 255, 255, 0.07), 0 12px 30px rgba(0, 0, 0, 0.5); }
@media (prefers-reduced-motion: reduce) {
  .pb-cta,
  .pb-cta-arrow { transition-duration: 0.01ms; }
  .pb-cta:hover,
  .pb-cta:focus-visible,
  .pb-cta:hover .pb-cta-arrow,
  .pb-cta:focus-visible .pb-cta-arrow { transform: none; }
}
`;

// The one piece of hardware in the product, and now it appears twice: on the
// homepage as the call to action and on the waitlist form as its submit. At
// rest the meter shows a noise floor (the same slop scrolling behind the
// frost); on hover the filter closes and only the passband survives, lit in the
// icon's brass. The animation is the product's own metaphor, which is the price
// of putting motion here at all.
//
// A HOOK RATHER THAN A COMPONENT because the pointer handlers have to sit on
// the button, not on the canvas inside it: the meter is ground beneath a label,
// and the geometry it reads is the button's own box. So the hook hands back
// both halves, the caller spreads the handlers onto whatever element it is
// building, and the two pages share one meter rather than growing two that
// drift apart.
function useMeter() {
  const canvasRef = useRef<HTMLCanvasElement>(null);
  // Hover lives in a ref, not state: the rAF loop reads it every frame and CSS
  // already owns the chrome, so a re-render would buy nothing.
  const hovered = useRef(false);
  // Where the passband is tuned to and how far it is opened, both 0..1 and both
  // driven by the pointer: x slides the band along the button, y opens it up.
  // Same reasoning as above: a ref, read per frame, never re-rendering.
  const tuned = useRef({ x: 0.5, lift: 1 });

  useEffect(() => {
    const canvas = canvasRef.current;
    const ctx = canvas?.getContext("2d");
    if (!canvas || !ctx) return;
    const reduce = matchMedia("(prefers-reduced-motion: reduce)").matches;

    // Each bar drifts on its own beat so the floor shimmers rather than
    // marching in step. Sized to the widest bar count the button can ask for.
    const POOL = 96;
    const phase = Array.from({ length: POOL }, () => Math.random() * Math.PI * 2);
    const speed = Array.from({ length: POOL }, () => 0.7 + Math.random() * 1.9);

    // The filter's frequency response: flat across the top, steep skirts either
    // side. This curve is the passband the product is named for. `c` is where it
    // is tuned to and `w` how wide it is opened.
    const band = (x: number, c: number, w: number) =>
      Math.exp(-Math.pow(Math.abs(x - c) / w, 4));

    let width = 0;
    let height = 0;
    let gate = 0; // 0 = wide open (noise), 1 = filtered down to the passband
    // The drawn centre and opening, chasing `tuned`. Eased rather than assigned
    // so a fast cursor pulls the band along instead of teleporting it, and so
    // leaving the button glides it home rather than snapping.
    let centre = 0.5;
    // Named `aperture`, not `open`: the bar loop below already has its own
    // local `open` for a bar's unfiltered height, and reading an outer `open`
    // earlier in that same block lands in the temporal dead zone and throws
    // on every frame. Syntax checks do not catch it; the canvas just stays
    // blank.
    let aperture = 1;

    const render = (t: number) => {
      ctx.clearRect(0, 0, width, height);
      // Width tracks height, so raising the cursor grows the hump rather than
      // stretching it: the skirts spread at the same rate the peak climbs and
      // the silhouette stays the same shape at every size.
      const bandW = FULL_W * aperture;
      const bars = Math.max(16, Math.min(POOL, Math.round(width / 5.5)));
      const step = width / bars;
      const barW = Math.max(1.5, step - 2);
      const maxH = height * 0.44;

      // Trace the response curve only once the filter is closing, so it reads
      // as the cause of the collapse rather than as decoration.
      if (gate > 0.01) {
        const top = (x: number) =>
          height - (0.06 + 0.94 * aperture * band(x / width, centre, bandW)) * maxH;
        ctx.beginPath();
        ctx.moveTo(0, height);
        for (let x = 0; x <= width; x += 2) ctx.lineTo(x, top(x));
        ctx.lineTo(width, height);
        const fill = ctx.createLinearGradient(0, height - maxH, 0, height);
        fill.addColorStop(0, `rgba(${BRASS}, ${0.16 * gate})`);
        fill.addColorStop(1, `rgba(${BRASS}, 0)`);
        ctx.fillStyle = fill;
        ctx.fill();

        ctx.beginPath();
        ctx.moveTo(0, top(0));
        for (let x = 2; x <= width; x += 2) ctx.lineTo(x, top(x));
        ctx.strokeStyle = `rgba(${BRASS}, ${0.45 * gate})`;
        ctx.lineWidth = 1;
        ctx.stroke();
      }

      for (let i = 0; i < bars; i++) {
        const x = bars > 1 ? i / (bars - 1) : 0.5;
        const response = aperture * band(x, centre, bandW);
        // Two incommensurate beats per bar: busy, but never repeating.
        const noise =
          0.5 +
          0.5 *
            Math.sin(t * speed[i] + phase[i]) *
            Math.cos(t * speed[i] * 0.61 + phase[i] * 1.7);
        const open = 0.12 + 0.3 * noise;
        const filtered = 0.04 + 0.96 * response * (0.55 + 0.45 * noise);
        const barH = Math.max(1, (open * (1 - gate) + filtered * gate) * maxH);

        // Warmth is gated response: only bars the filter passes light up.
        const warm = gate * response;
        const mix = (cold: number, hot: number) =>
          Math.round(cold + (hot - cold) * warm);
        ctx.fillStyle = `rgba(${mix(124, 240)}, ${mix(124, 204)}, ${mix(134, 128)}, ${0.4 + 0.55 * warm})`;
        ctx.shadowBlur = warm > 0.25 ? 10 * warm : 0;
        ctx.shadowColor = `rgba(${BRASS}, ${0.7 * warm})`;
        const bx = i * step + (step - barW) / 2;
        ctx.beginPath();
        ctx.roundRect(bx, height - barH, barW, barH, barW / 2);
        ctx.fill();
      }
      ctx.shadowBlur = 0;
    };

    const resize = () => {
      const dpr = Math.min(2, window.devicePixelRatio || 1);
      const rect = canvas.getBoundingClientRect();
      width = rect.width;
      height = rect.height;
      canvas.width = Math.round(width * dpr);
      canvas.height = Math.round(height * dpr);
      ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
      // Resizing clears the bitmap, and with no loop running nothing would
      // repaint it.
      if (reduce) render(0);
    };

    let raf = 0;
    let last = 0;
    const loop = (now: number) => {
      const dt = last ? Math.min(0.05, (now - last) / 1000) : 0;
      last = now;
      // Exponential approach, so the ease is the same at 60Hz and 120Hz.
      gate += ((hovered.current ? 1 : 0) - gate) * (1 - Math.exp(-dt * 9));
      // Tuning tracks faster than the filter opens: the band should feel
      // attached to the cursor, while the collapse into it stays a beat behind.
      centre += (tuned.current.x - centre) * (1 - Math.exp(-dt * 16));
      aperture += (tuned.current.lift - aperture) * (1 - Math.exp(-dt * 16));
      render(now / 1000);
      raf = requestAnimationFrame(loop);
    };

    resize();
    const observer = new ResizeObserver(resize);
    observer.observe(canvas);
    // Reduced motion still gets the meter, just held on a single frame.
    if (reduce) render(0);
    else raf = requestAnimationFrame(loop);

    return () => {
      cancelAnimationFrame(raf);
      observer.disconnect();
    };
  }, []);

  return {
    // Everything that goes INSIDE the button: the stylesheet, and the canvas
    // the loop above draws on.
    chrome: (
      <>
        {/* href + precedence is React 19's hoist path: the rules land in <head>
            and dedupe by href instead of sitting loose in the body, which is
            also what lets both buttons render this without shipping it twice. */}
        <style href="pb-cta" precedence="default">
          {CTA_CSS}
        </style>
        <canvas ref={canvasRef} className="pb-cta-meter" aria-hidden="true" />
      </>
    ),
    // Everything that goes ON it.
    handlers: {
      onPointerEnter: () => {
        hovered.current = true;
      },
      onPointerMove: (event: PointerEvent<HTMLElement>) => {
        const box = event.currentTarget.getBoundingClientRect();
        const x = (event.clientX - box.left) / (box.width || 1);
        // Screen y grows downward and the hump grows upward, so invert: the
        // top of the button is the filter wide open.
        const lift = 1 - (event.clientY - box.top) / (box.height || 1);
        // Never fully shut: at zero the hump has no height and no width, so
        // the bars all die and the button looks broken rather than tuned. The
        // top of the travel goes past 1, which is the meter's nominal full
        // height, so the peak reaches up behind the label instead of stopping
        // politely beneath it.
        const aperture = 0.38 + 0.8 * Math.min(1, Math.max(0, lift));
        // Clamped by the skirts' real width, not a fixed margin: a wide hump
        // needs more room to keep both shoulders on the button than a narrow
        // one, so the travel opens up exactly as the filter closes down. The
        // quartic is down to a percent of peak by 1.5 half-widths, so 0.72 is
        // where the shoulder has visually landed.
        const edge = 0.72 * FULL_W * aperture;
        tuned.current = {
          x: Math.min(1 - edge, Math.max(edge, x)),
          lift: aperture,
        };
      },
      onPointerLeave: () => {
        hovered.current = false;
        // Home, so the next hover starts centred and open rather than
        // wherever the last one happened to end.
        tuned.current = { x: 0.5, lift: 1 };
      },
      // Keyboard focus has no cursor to follow, so it gets the centred band.
      onFocus: () => {
        hovered.current = true;
      },
      onBlur: () => {
        hovered.current = false;
      },
    },
  };
}

// The arrow both buttons wear. It points the way the press goes, which is
// onward now rather than down: the homepage leads to the waitlist, and the
// waitlist form sends. Label first, arrow second, so the two read left to right
// in the order they happen.
function Arrow() {
  return (
    <svg
      className="pb-cta-arrow"
      width="15"
      height="14"
      viewBox="0 0 15 14"
      fill="none"
      stroke="currentColor"
      strokeWidth="1.6"
      strokeLinecap="round"
      strokeLinejoin="round"
      aria-hidden="true"
    >
      <path d="M1.4 7h11.2m0 0L9.3 3.7M12.6 7 9.3 10.3" />
    </svg>
  );
}

// THE HOMEPAGE'S ONE ACTION. It used to be the download, and the client is not
// a thing worth holding before there is a mailbox behind it: the door here is
// the list, and the download waits at the end of the invite flow, where it is
// the next thing somebody actually needs.
function JoinButton() {
  const { chrome, handlers } = useMeter();
  return (
    <a className="pb-cta" href="/waitlist" {...handlers}>
      {chrome}
      <span className="pb-cta-label">join the waitlist</span>
      <Arrow />
    </a>
  );
}

// The same hardware as a form submit, spanning the field above it. `busy` is
// the request in flight: the meter keeps running, because that is the part that
// is honestly still happening, and the button stops answering the pointer.
function SubmitButton({ busy }: { busy: boolean }) {
  const { chrome, handlers } = useMeter();
  return (
    <button
      className="pb-cta pb-cta-wide"
      type="submit"
      disabled={busy}
      {...handlers}
    >
      {chrome}
      <span className="pb-cta-label">{busy ? "sending" : "join"}</span>
      {!busy && <Arrow />}
    </button>
  );
}

// Same corner links on every React page, so /waitlist reads as a sibling of
// the homepage rather than a detour off the site.
function CornerLinks() {
  return (
    <>
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
    </>
  );
}

export function App() {
  return (
    <main style={styles.page}>
      <FakeInbox />
      <div style={styles.frost} />
      <div style={styles.content}>
        {/* The mark on its own, no tile: the page is already a dark field, so
            the icon's ground would just be a lighter rectangle sitting on it. */}
        <img
          src="/mark.svg"
          alt="Passband"
          width={180}
          height={98}
        />
        <h1 style={styles.title}>Passband</h1>
        <p style={styles.tagline}>fuck email. lets make it bearable</p>
        <JoinButton />
        {/* The second door, for the people the button is not for. Muted, and
            a line rather than a button, because there is one primary action on
            this page and this is not it. */}
        <p style={styles.note}>
          already have an invite?{" "}
          <a style={styles.noteLink} href={SIGNUP_URL}>
            set up your mailbox
          </a>
        </p>
      </div>
      <CornerLinks />
    </main>
  );
}

// The control plane answers 200 for a fresh address and for one already on the
// list, so this page can never become a membership oracle.
const WAITLIST_URL = "https://signup.passband.app/waitlist";

// Where an invite is redeemed. The emailed link goes straight here with the
// code in it; this is the way in for somebody who has the mail open on another
// device, or who lost it and kept the code.
const SIGNUP_URL = "https://signup.passband.app";

export function WaitlistPage() {
  const [email, setEmail] = useState("");
  const [state, setState] = useState<"idle" | "busy" | "done" | "error">("idle");

  const submit = async (event: FormEvent) => {
    event.preventDefault();
    if (state === "busy") return;
    setState("busy");
    try {
      const res = await fetch(WAITLIST_URL, {
        method: "POST",
        // urlencoded keeps this a CORS simple request: no preflight round trip.
        headers: { "Content-Type": "application/x-www-form-urlencoded" },
        body: new URLSearchParams({ email }),
        credentials: "omit",
      });
      setState(res.ok ? "done" : "error");
    } catch {
      setState("error");
    }
  };

  return (
    <main style={styles.page}>
      <FakeInbox />
      <div style={styles.frost} />
      <div style={styles.content}>
        <a
          href="/"
          style={{
            display: "flex",
            flexDirection: "column",
            alignItems: "center",
            gap: "1rem",
            textDecoration: "none",
          }}
        >
          <img
            src="/mark.svg"
            alt="Passband"
            width={180}
            height={98}
          />
          <h1 style={styles.title}>Passband</h1>
        </a>
        {/* THE WHOLE CARD TURNS OVER ON SUCCESS, pitch included. Leaving the
            pitch standing above the confirmation left the card selling the list
            to somebody who had just joined it, and saying "your invite lands by
            email" twice in four lines. What replaces it is the only thing still
            unanswered at that point: what arrives, and what is at the end of
            it, which is where the client lives now. */}
        <section style={{ ...styles.card, marginTop: "0.5rem" }}>
          {state === "done" ? (
            <>
              <span style={styles.cardLabel}>you're on the list</span>
              <p style={styles.cardCopy}>
                the invite lands by email when a spot opens. it walks you
                through setup, and the app is waiting at the end of it.
              </p>
            </>
          ) : (
            <>
              <span style={styles.cardLabel}>hosted beta</span>
              <p style={styles.cardCopy}>
                we run the daemon for you. join the list and your invite lands
                by email.
              </p>
              <form style={styles.waitlistForm} onSubmit={submit}>
                <input
                  style={styles.waitlistInput}
                  type="email"
                  name="email"
                  required
                  autoComplete="email"
                  placeholder="you@example.com"
                  aria-label="email address"
                  value={email}
                  disabled={state === "busy"}
                  onChange={(event) => setEmail(event.target.value)}
                />
                <SubmitButton busy={state === "busy"} />
              </form>
            </>
          )}
          {state === "error" && (
            <p style={{ ...styles.status, color: "#d8a39a" }}>
              that didn't go through. give it a second and try again.
            </p>
          )}
        </section>
        {/* Same second door as the homepage, for somebody who followed the
            join link with a code already sitting in their inbox. */}
        <p style={styles.note}>
          already have an invite?{" "}
          <a style={styles.noteLink} href={SIGNUP_URL}>
            set up your mailbox
          </a>
        </p>
      </div>
      <CornerLinks />
    </main>
  );
}
