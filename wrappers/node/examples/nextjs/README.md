# sekejap + Next.js

A minimal App Router project with an embedded sekejap database behind a route
handler. The one thing Next.js needs is `serverExternalPackages` in
`next.config.js` — sekejap's engine is a native addon that Node loads from
disk, so the bundler must leave it external (same as better-sqlite3 or sharp).

```sh
npm install
npm run build
npm run start          # port 3457
curl 'localhost:3457/api/near?m=20000'
```

Works in the Node runtime (route handlers, API routes, server components,
server actions). Not the Edge runtime, and not client components — keep
sekejap calls server-side.
