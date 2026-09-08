import { Database } from "bun:sqlite";

const port = Number(process.env.PORT ?? process.env.NIBRUN_HTTP_PORT ?? 3000);
const dataDir = process.env.DATA_DIR ?? "/app/data";

const db = new Database(`${dataDir}/todos.db`, { create: true });
db.exec("PRAGMA journal_mode = WAL");
db.exec("PRAGMA synchronous = NORMAL");
db.exec(
  "CREATE TABLE IF NOT EXISTS todos (id INTEGER PRIMARY KEY AUTOINCREMENT, title TEXT NOT NULL, done INTEGER NOT NULL DEFAULT 0, created_at TEXT NOT NULL DEFAULT (datetime('now')))",
);

const listTodos = db.query("SELECT id, title, done, created_at FROM todos ORDER BY id DESC LIMIT 100");
const insertTodo = db.query("INSERT INTO todos (title) VALUES (?) RETURNING id, title, done, created_at");

const boots = db.query("SELECT count(*) AS n FROM todos").get() as { n: number };

const json = (body: unknown, status = 200) =>
  new Response(JSON.stringify(body), { status, headers: { "content-type": "application/json" } });

Bun.serve({
  port,
  hostname: "0.0.0.0",
  fetch(request) {
    const { pathname } = new URL(request.url);

    if (pathname === "/health") return new Response("ok\n");

    if (pathname === "/todos") {
      if (request.method === "GET") return json(listTodos.all());
      if (request.method === "POST") {
        return request
          .json()
          .then((body: { title?: unknown }) => {
            const title = typeof body?.title === "string" ? body.title.trim() : "";
            if (!title) return json({ error: "title is required" }, 400);
            return json(insertTodo.get(title), 201);
          })
          .catch(() => json({ error: "body must be JSON" }, 400));
      }
      return json({ error: "method not allowed" }, 405);
    }

    return json({ error: "not found" }, 404);
  },
});

console.log(`listening on ${port}, ${boots.n} todos already on the volume at ${dataDir}`);
