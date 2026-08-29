import { useEffect, useState } from "react";
import { createUser, listUsers, UserEntry } from "../api";
import { ErrorBox, Loading } from "./util";

export default function Users() {
  const [users, setUsers] = useState<UserEntry[] | null>(null);
  const [error, setError] = useState<Error | null>(null);
  const [username, setUsername] = useState("");
  const [password, setPassword] = useState("");
  const [role, setRole] = useState("operator");
  const [busy, setBusy] = useState(false);
  const [notice, setNotice] = useState("");

  useEffect(() => {
    listUsers().then(setUsers).catch(setError);
  }, []);

  const submit = async (e: React.FormEvent) => {
    e.preventDefault();
    setError(null);
    setBusy(true);
    try {
      await createUser(username.trim(), password, role);
      setNotice(`User ${username.trim()} (${role}) created.`);
      setUsername("");
      setPassword("");
      setUsers(await listUsers());
    } catch (err) {
      setError(err as Error);
    } finally {
      setBusy(false);
    }
  };

  return (
    <section>
      <h2>Users</h2>
      {error && <ErrorBox err={error} />}
      {notice && <p className="muted">{notice}</p>}
      {users === null ? (
        <Loading />
      ) : (
        <table>
          <thead>
            <tr>
              <th>Username</th>
              <th>Role</th>
            </tr>
          </thead>
          <tbody>
            {users.map((u) => (
              <tr key={u.username}>
                <td>{u.username}</td>
                <td>{u.role}</td>
              </tr>
            ))}
          </tbody>
        </table>
      )}
      <details className="advanced" style={{ marginTop: 16 }}>
        <summary>Create user</summary>
        <form onSubmit={submit} className="form">
          <label>
            Username
            <input
              value={username}
              onChange={(e) => setUsername(e.target.value)}
              required
            />
          </label>
          <label>
            Password
            <input
              type="password"
              value={password}
              onChange={(e) => setPassword(e.target.value)}
              required
            />
          </label>
          <label>
            Role
            <select value={role} onChange={(e) => setRole(e.target.value)}>
              <option value="operator">operator</option>
              <option value="admin">admin</option>
            </select>
          </label>
          <button type="submit" disabled={busy}>
            {busy ? "Creating…" : "Create user"}
          </button>
        </form>
      </details>
    </section>
  );
}
