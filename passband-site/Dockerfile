# Landing page. The Railway service's root directory is passband-site, so this
# builds with the site directory as its context and the root railway.toml
# (relay config, see deploy/DEPLOY.md §8) never applies to this service.
FROM oven/bun:1-slim
WORKDIR /app

COPY package.json bun.lockb ./
RUN bun install --frozen-lockfile

COPY . .
RUN bun run build

ENV NODE_ENV=production
EXPOSE 3000
CMD ["bun", "run", "server.ts"]
