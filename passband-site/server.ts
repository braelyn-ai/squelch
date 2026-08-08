// Static pages are plain HTML files, not React routes: legal pages must be
// readable by crawlers and reviewers without executing the bundle.
const pages: Record<string, string> = {
  "/main.js": "dist/main.js",
  "/privacy": "privacy.html",
  "/terms": "terms.html",
};

const server = Bun.serve({
  port: Number(process.env.PORT ?? 3000),
  async fetch(req) {
    const path = new URL(req.url).pathname;
    const file = pages[path];
    if (file) {
      return new Response(Bun.file(file));
    }
    return new Response(Bun.file("index.html"));
  },
});

console.log(`passband-site listening on ${server.url}`);
