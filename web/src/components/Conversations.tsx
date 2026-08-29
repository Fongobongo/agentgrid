import { useEffect, useRef, useState } from "react";
import {
  appendConversationMessage,
  Conversation,
  ConversationMessage,
  createConversation,
  listConversationMessages,
  listConversations,
} from "../api";
import { ErrorBox, Loading, fmtTime } from "./util";

export default function Conversations() {
  const [convs, setConvs] = useState<Conversation[] | null>(null);
  const [error, setError] = useState<Error | null>(null);
  const [selected, setSelected] = useState<Conversation | null>(null);
  const [messages, setMessages] = useState<ConversationMessage[] | null>(null);
  const [draft, setDraft] = useState("");
  const [adapter, setAdapter] = useState("mock");
  const [repository, setRepository] = useState("");
  const [busy, setBusy] = useState(false);
  const bottom = useRef<HTMLDivElement>(null);

  const refresh = () => listConversations().then(setConvs).catch(setError);

  useEffect(() => {
    refresh();
  }, []);

  useEffect(() => {
    setMessages(null);
    if (!selected) return;
    listConversationMessages(selected.id)
      .then(setMessages)
      .catch(() => setMessages([]));
  }, [selected]);

  useEffect(() => {
    bottom.current?.scrollIntoView({ block: "end" });
  }, [messages]);

  const send = async (e: React.FormEvent) => {
    e.preventDefault();
    if (!selected || !draft.trim()) return;
    setBusy(true);
    try {
      await appendConversationMessage(selected.id, draft.trim());
      setDraft("");
      setMessages(await listConversationMessages(selected.id));
      refresh();
    } catch (err) {
      setError(err as Error);
    } finally {
      setBusy(false);
    }
  };

  const start = async (e: React.FormEvent) => {
    e.preventDefault();
    setError(null);
    setBusy(true);
    try {
      const c = await createConversation(
        adapter.trim(),
        repository.trim() || undefined,
      );
      await refresh();
      setSelected(c as Conversation);
    } catch (err) {
      setError(err as Error);
    } finally {
      setBusy(false);
    }
  };

  return (
    <section>
      <h2>Conversations</h2>
      <p className="muted">
        Multi-turn chat routed through the CP — every user message becomes a
        task whose prompt is the composed history.
      </p>
      {error && <ErrorBox err={error} />}
      {convs === null ? (
        <Loading />
      ) : (
        <table>
          <thead>
            <tr>
              <th>ID</th>
              <th>Adapter</th>
              <th>Repository</th>
              <th>Created</th>
            </tr>
          </thead>
          <tbody>
            {convs.map((c) => (
              <tr
                key={c.id}
                onClick={() => setSelected(c)}
                style={{
                  cursor: "pointer",
                  background: selected?.id === c.id ? "var(--bg2)" : undefined,
                }}
              >
                <td>{c.id.slice(0, 8)}…</td>
                <td>{c.adapter}</td>
                <td>{c.repository || "—"}</td>
                <td>{fmtTime(c.created_at)}</td>
              </tr>
            ))}
          </tbody>
        </table>
      )}

      {selected && (
        <section style={{ marginTop: 16 }} className="conversation">
          <h3>Conversation {selected.id.slice(0, 8)}…</h3>
          {messages === null ? (
            <Loading />
          ) : (
            <div
              className="msg-list"
              style={{
                maxHeight: 400,
                overflowY: "auto",
                display: "flex",
                flexDirection: "column",
                gap: 8,
              }}
            >
              {messages.map((m) => (
                <div key={m.seq} className="msg">
                  <b>{m.role}</b>{" "}
                  {m.task_id && (
                    <a href={`#/task/${m.task_id}`} className="muted">
                      task {m.task_id.slice(0, 8)}…
                    </a>
                  )}{" "}
                  <span className="muted">{fmtTime(m.created_at)}</span>
                  <pre style={{ whiteSpace: "pre-wrap", margin: "2px 0 0" }}>
                    {m.content}
                  </pre>
                </div>
              ))}
              <div ref={bottom} />
            </div>
          )}
          <form onSubmit={send} className="form" style={{ marginTop: 10 }}>
            <label>
              Message
              <textarea
                rows={2}
                value={draft}
                onChange={(e) => setDraft(e.target.value)}
              />
            </label>
            <button type="submit" disabled={busy}>
              {busy ? "Sending…" : "Send"}
            </button>
          </form>
        </section>
      )}

      <details className="advanced" style={{ marginTop: 16 }}>
        <summary>New conversation</summary>
        <form onSubmit={start} className="form">
          <label>
            Adapter
            <input
              value={adapter}
              onChange={(e) => setAdapter(e.target.value)}
              required
            />
          </label>
          <label>
            Repository (optional)
            <input
              value={repository}
              onChange={(e) => setRepository(e.target.value)}
            />
          </label>
          <button type="submit" disabled={busy}>
            {busy ? "Creating…" : "Create conversation"}
          </button>
        </form>
      </details>
    </section>
  );
}
