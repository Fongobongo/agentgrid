import { useEffect, useState } from "react";
import { createRepository, listRepos, RepositoryView } from "../api";
import { ErrorBox, Loading, fmtTime } from "./util";

export default function Repositories() {
  const [repos, setRepos] = useState<RepositoryView[] | null>(null);
  const [error, setError] = useState<Error | null>(null);
  const [name, setName] = useState("");
  const [gitUrl, setGitUrl] = useState("");
  const [branch, setBranch] = useState("main");
  const [validation, setValidation] = useState("");
  const [busy, setBusy] = useState(false);

  const refresh = () => listRepos().then(setRepos).catch(setError);

  useEffect(() => {
    refresh();
  }, []);

  const submit = async (e: React.FormEvent) => {
    e.preventDefault();
    setError(null);
    setBusy(true);
    try {
      await createRepository({
        name: name.trim(),
        git_url: gitUrl.trim(),
        default_branch: branch.trim() || "main",
        validation_command: validation.trim() || undefined,
      });
      setName("");
      setGitUrl("");
      setValidation("");
      await refresh();
    } catch (err) {
      setError(err as Error);
    } finally {
      setBusy(false);
    }
  };

  return (
    <section>
      <h2>Repositories</h2>
      {error && <ErrorBox err={error} />}
      {repos === null ? (
        <Loading />
      ) : (
        <table>
          <thead>
            <tr>
              <th>Name</th>
              <th>Git URL</th>
              <th>Branch</th>
              <th>Validation</th>
              <th>Added</th>
            </tr>
          </thead>
          <tbody>
            {repos.map((r) => (
              <tr key={r.id}>
                <td>{r.name}</td>
                <td>
                  <code>{r.git_url}</code>
                </td>
                <td>{r.default_branch}</td>
                <td>{r.validation_command ?? "—"}</td>
                <td>{fmtTime(r.created_at)}</td>
              </tr>
            ))}
          </tbody>
        </table>
      )}
      <details className="advanced" style={{ marginTop: 16 }}>
        <summary>Add repository</summary>
        <form onSubmit={submit} className="form">
          <label>
            Name
            <input
              value={name}
              onChange={(e) => setName(e.target.value)}
              required
            />
          </label>
          <label>
            Git URL
            <input
              value={gitUrl}
              onChange={(e) => setGitUrl(e.target.value)}
              placeholder="https://github.com/you/repo"
              required
            />
          </label>
          <label>
            Default branch
            <input
              value={branch}
              onChange={(e) => setBranch(e.target.value)}
              placeholder="main"
            />
          </label>
          <label>
            Validation command <span className="muted">(optional)</span>
            <input
              value={validation}
              onChange={(e) => setValidation(e.target.value)}
              placeholder="cargo test"
            />
          </label>
          <button type="submit" disabled={busy}>
            {busy ? "Adding…" : "Add repository"}
          </button>
        </form>
      </details>
    </section>
  );
}
