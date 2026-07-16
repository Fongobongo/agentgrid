import { useState } from 'react';
import { ApiError, login, setup } from '../api';

export default function Login({ onAuthed }: { onAuthed: (t: string) => void }) {
  const [mode, setMode] = useState<'login' | 'setup'>('login');
  const [username, setUsername] = useState('');
  const [password, setPassword] = useState('');
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  const submit = async (e: React.FormEvent) => {
    e.preventDefault();
    setError(null);
    setBusy(true);
    try {
      const res =
        mode === 'login'
          ? await login(username, password)
          : await setup(username, password);
      onAuthed(res.token);
    } catch (err) {
      if (err instanceof ApiError) {
        if (err.status === 409 && mode === 'setup') {
          setError('An account already exists. Switch to Login.');
          setMode('login');
        } else if (err.status === 401) {
          setError('Invalid username or password.');
        } else {
          setError(`Request failed (${err.status}).`);
        }
      } else {
        setError('Network error.');
      }
    } finally {
      setBusy(false);
    }
  };

  return (
    <div className="login-wrap">
      <form className="login-card" onSubmit={submit}>
        <h1>agentgrid</h1>
        <p className="muted">
          {mode === 'setup' ? 'Create the first admin account' : 'Sign in to continue'}
        </p>
        <label>
          Username
          <input
            value={username}
            onChange={(e) => setUsername(e.target.value)}
            autoFocus
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
        {error && <div className="error">{error}</div>}
        <button type="submit" disabled={busy}>
          {mode === 'setup' ? 'Create admin' : 'Login'}
        </button>
        <button
          type="button"
          className="link"
          onClick={() => {
            setMode(mode === 'login' ? 'setup' : 'login');
            setError(null);
          }}
        >
          {mode === 'login'
            ? 'No account yet? Create the first admin.'
            : 'Have an account? Switch to login.'}
        </button>
      </form>
    </div>
  );
}
