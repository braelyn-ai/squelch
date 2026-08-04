const server = Bun.serve({
  port: Number(process.env.PORT ?? 3000),
  async fetch(req) {
    const path = new URL(req.url).pathname;
    if (path === "/main.js") {
      return new Response(Bun.file("dist/main.js"));
    }
    return new Response(Bun.file("index.html"));
  },
});

console.log(`passband-site listening on ${server.url}`);
