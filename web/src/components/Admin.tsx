import { useState } from "react";
import { adminBackup, ApiError, storageGc, StorageGcResult } from "../api";
import { ErrorBox } from "./util";

export default function Admin() {
  const [path, setPath] = useState("");
  const [gcResult, setGcResult] = useState<StorageGcResult | null>(null);
  const [notice, setNotice] = useState("");
  const [error, setError] = useState<Error | null>(null);
  const [busy, setBusy] = useState<string | null>(null);

  const backup = async (e: React.FormEvent) => {
    e.preventDefault();
    setError(null);
    setNotice("");
    setBusy("backup");
    try {
      if (!path.trim())
        throw new Error(
          "path required (relative to artifact root, e.g. backup.sqlite3)",
        );
      await adminBackup(path.trim());
      setNotice(`Backup written to ${path.trim()}.`);
    } catch (err) {
      setError(err as Error);
    } finally {
      setBusy(null);
    }
  };

  const runGc = async (dry: boolean) => {
    setError(null);
    setNotice("");
    setBusy("gc");
    try {
      const r = await storageGc(dry);
      setGcResult(r);
    } catch (err) {
      if (err instanceof ApiError)
        setError(new Error(`storage.gc: ${err.message}`));
      else setError(err as Error);
    } finally {
      setBusy(null);
    }
  };

  return (
    <section>
      <h2>Admin</h2>
      {error && <ErrorBox err={error} />}
      {notice && <p className="muted">{notice}</p>}

      <h3>Backup</h3>
      <p className="muted">
        SQLite VACUUM INTO — copies the live DB to a path relative to the data
        dir.
      </p>
      <form onSubmit={backup} className="form" style={{ maxWidth: 460 }}>
        <label>
          Path (relative to artifact root)
          <input
            value={path}
            onChange={(e) => setPath(e.target.value)}
            placeholder="backups/backup.sqlite3"
            required
          />
        </label>
        <button type="submit" disabled={!!busy}>
          {busy === "backup" ? "Backing up…" : "Write backup"}
        </button>
      </form>

      <h3 style={{ marginTop: 24 }}>Storage GC</h3>
      <p className="muted">
        Reconcile artifacts vs metadata. Dry-run lists orphan files / dangling
        rows without deleting.
      </p>
      <p>
        <button onClick={() => runGc(true)} disabled={busy !== null}>
          Dry run
        </button>{" "}
        <button
          className="danger"
          onClick={() => runGc(false)}
          disabled={busy !== null}
        >
          {busy === "gc" ? "Reconciling…" : "Run GC"}
        </button>
      </p>
      {gcResult && (
        <ul>
          <li>
            Orphan files: {gcResult.orphan_files} ({gcResult.orphan_bytes}{" "}
            bytes)
          </li>
          <li>Metadata without file: {gcResult.metadata_without_file}</li>
          <li>Free: {gcResult.free_mb} MiB</li>
        </ul>
      )}
    </section>
  );
}
