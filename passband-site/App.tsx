const styles = {
  page: {
    minHeight: "100vh",
    display: "flex",
    flexDirection: "column",
    alignItems: "center",
    justifyContent: "center",
    gap: "1rem",
    margin: 0,
    fontFamily: "system-ui, sans-serif",
  },
  title: {
    fontSize: "3rem",
    fontWeight: 600,
    margin: 0,
  },
  link: {
    color: "#555",
    fontSize: "1rem",
  },
} as const;

export function App() {
  return (
    <main style={styles.page}>
      <h1 style={styles.title}>Passband</h1>
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
    </main>
  );
}
