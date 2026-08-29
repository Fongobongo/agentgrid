import { useEffect, useState } from "react";
import {
  deleteSharedContext,
  listSharedContext,
  setSharedContext,
  SharedContextEntry,
} from "../api";
import { ErrorBox, fmtTime } from "./util";

export default function SharedContext() {
  const [groupId, setGroupId] = useState("");
  const [activeGroup, setActiveGroup] = useState<string | null>(null);
  const [entries, setEntries] = useState<SharedContextEntry[] | null>(null);
  const [key, setKey] = useState("");
  const [value, setValue] = useState("");
  const [error, setError] = useState<Error | null>(null);

  const load = (g: string) => {
    setError(null);
    listSharedContext(g)
      .then(setEntries)
      .catch((e) => {
        setEntries(null);
        setError(e);
      });
  };

  useEffect(() => {
    if (activeGroup !== null) load(activeGroup);
  }, [activeGroup]);

  const open = (e: React.FormEvent) => {
    e.preventDefault();
    if (groupId.trim()) setActiveGroup(groupId.trim());
  };

  const setEntry = async (e: React.FormEvent) => {
    e.preventDefault();
    if (!activeGroup || !key.trim()) return;
    await setSharedContext(activeGroup, key.trim(), value).catch(setError);
    setKey("");
    setValue("");
    load(activeGroup);
  };

  return (
    <section>
      <h2>Shared context</h2>
      <p className="muted">
        Plan 1.12 (#7) — task-group key/value notes visible to every attempt in
        the group.
      </p>
      {error && <ErrorBox err={error} />}
      <form onSubmit={open} className="form" style={{ maxWidth: 320 }}>
        <label>
          Group id
          <input
            value={groupId}
            onChange={(e) => setGroupId(e.target.value)}
            placeholder="group-1"
            required
          />
        </label>
        <button type="submit">Open group</button>
      </form>
      {activeGroup !== null && (
        <>
          <h3 style={{ marginTop: 16 }}>Notes — {activeGroup}</h3>
          {entries === null ? (
            <p className="muted">Not found / none yet.</p>
          ) : (
            <table>
              <thead>
                <tr>
                  <th>Key</th>
                  <th>Value</th>
                  <th>Updated</th>
                  <th></th>
                </tr>
              </thead>
              <tbody>
                {entries.map((e) => (
                  <tr key={e.key}>
                    <td>{e.key}</td>
                    <td style={{ maxWidth: 400 }}>
                      <pre style={{ margin: 0, whiteSpace: "pre-wrap" }}>
                        {e.value}
                      </pre>
                    </td>
                    <td>{fmtTime(e.updated_at)}</td>
                    <td>
                      <button
                        className="danger"
                        onClick={() => {
                          deleteSharedContext(activeGroup, e.key)
                            .then(() => load(activeGroup))
                            .catch(setError);
                        }}
                      >
                        Delete
                      </button>
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          )}
          <form onSubmit={setEntry} className="form" style={{ marginTop: 12 }}>
            <label>
              Key
              <input
                value={key}
                onChange={(e) => setKey(e.target.value)}
                required
              />
            </label>
            <label>
              Value
              <textarea
                rows={2}
                value={value}
                onChange={(e) => setValue(e.target.value)}
              />
            </label>
            <button type="submit">Set</button>
          </form>
        </>
      )}
    </section>
  );
}
