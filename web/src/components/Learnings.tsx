import { useEffect, useState } from "react";
import {
  listRepos,
  listLearnings,
  addLearning,
  setLearningApproved,
  deleteLearning,
  RepoLearning,
  RepositoryView,
} from "../api";
import { ErrorBox, Loading, fmtTime } from "./util";

export default function Learnings() {
  const [repos, setRepos] = useState<RepositoryView[]>([]);
  const [repo, setRepo] = useState("");
  const [items, setItems] = useState<RepoLearning[] | null>(null);
  const [statement, setStatement] = useState("");
  const [error, setError] = useState<Error | null>(null);

  useEffect(() => {
    listRepos().then(setRepos).catch(setError);
  }, []);

  useEffect(() => {
    const name = repos[0]?.name;
    if (name && !repo) setRepo(name);
  }, [repos]);

  useEffect(() => {
    if (!repo) return;
    listLearnings(repo).then(setItems).catch(setError);
  }, [repo]);

  const refresh = () => {
    if (!repo) return;
    listLearnings(repo)
      .then(setItems)
      .catch(() => {});
  };

  const submit = async (e: React.FormEvent) => {
    e.preventDefault();
    if (!repo || !statement.trim()) return;
    await addLearning(repo, statement.trim()).catch(setError);
    setStatement("");
    refresh();
  };

  return (
    <section>
      <h2>Learnings</h2>
      <p className="muted">
        Curated repo learnings injected into attempt context (Stage 7).
      </p>
      {error && <ErrorBox err={error} />}
      <p>
        Repo:{" "}
        <select value={repo} onChange={(e) => setRepo(e.target.value)}>
          {repos.map((r) => (
            <option key={r.id} value={r.name}>
              {r.name}
            </option>
          ))}
        </select>
      </p>
      {items === null ? (
        <Loading />
      ) : (
        <table>
          <thead>
            <tr>
              <th>Statement</th>
              <th>Confidence</th>
              <th>Approved</th>
              <th></th>
            </tr>
          </thead>
          <tbody>
            {items.map((l) => (
              <tr key={l.id}>
                <td>{l.statement}</td>
                <td>{l.confidence}</td>
                <td>{l.approved ? "✓" : "pending"}</td>
                <td style={{ whiteSpace: "nowrap" }}>
                  <button
                    onClick={() =>
                      setLearningApproved(l.id, !l.approved)
                        .then(refresh)
                        .catch(setError)
                    }
                  >
                    {l.approved ? "Revoke" : "Approve"}
                  </button>{" "}
                  <button
                    className="danger"
                    onClick={() =>
                      deleteLearning(l.id).then(refresh).catch(setError)
                    }
                  >
                    Delete
                  </button>
                </td>
              </tr>
            ))}
          </tbody>
        </table>
      )}
      <form onSubmit={submit} className="form" style={{ marginTop: 16 }}>
        <label>
          New learning (statement)
          <textarea
            rows={2}
            value={statement}
            onChange={(e) => setStatement(e.target.value)}
          />
        </label>
        <button type="submit">Add learning</button>
      </form>
      <p className="muted" style={{ marginTop: 8 }}>
        Updated: {fmtTime(items?.[0]?.updated_at ?? "")}
      </p>
    </section>
  );
}
