FROM oven/bun:1 AS build
WORKDIR /app
COPY package.json bun.lock ./
RUN bun install --frozen-lockfile
COPY . .

FROM oven/bun:1-slim
WORKDIR /app
COPY --from=build /app /app
ENV PORT=8000
EXPOSE 8000
CMD ["bun", "run", "src/index.ts"]
