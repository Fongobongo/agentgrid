import { useEffect, useState } from 'react';
import { ApiError, clearToken, getToken, setToken } from './api';
import Login from './components/Login';
import Dashboard from './components/Dashboard';
import Nodes from './components/Nodes';
import NewTask from './components/NewTask';
import TaskDetails from './components/TaskDetails';

function parseHash(): { name: string; id?: string } {
  const h = window.location.hash.replace(/^#\/?/, '');
  const parts = h.split('/');
  if (parts[0] === 'nodes') return { name: 'nodes' };
  if (parts[0] === 'new') return { name: 'new' };
  if (parts[0] === 'task' && parts[1]) return { name: 'task', id: parts[1] };
  return { name: 'dashboard' };
}

export default function App() {
  const [token, setTokenState] = useState<string | null>(getToken());
  const [route, setRoute] = useState(parseHash());

  useEffect(() => {
    const onHash = () => setRoute(parseHash());
    window.addEventListener('hashchange', onHash);
    return () => window.removeEventListener('hashchange', onHash);
  }, []);

  const logout = () => {
    clearToken();
    setTokenState(null);
  };

  const onAuthed = (t: string) => {
    setToken(t);
    setTokenState(t);
    window.location.hash = '#/';
  };

  if (!token) return <Login onAuthed={onAuthed} />;

  const nav = (to: string) => () => {
    window.location.hash = to;
  };
  const cls = (name: string) => 'navbtn' + (route.name === name ? ' active' : '');

  return (
    <div className="app">
      <header className="topbar">
        <span className="brand">agentgrid</span>
        <nav>
          <button className={cls('dashboard')} onClick={nav('#/')}>Dashboard</button>
          <button className={cls('nodes')} onClick={nav('#/nodes')}>Nodes</button>
          <button className={cls('new')} onClick={nav('#/new')}>New Task</button>
        </nav>
        <button className="navbtn logout" onClick={logout}>Logout</button>
      </header>
      <main className="content">
        {route.name === 'dashboard' && <Dashboard onOpen={(id) => (window.location.hash = `#/task/${id}`)} />}
        {route.name === 'nodes' && <Nodes />}
        {route.name === 'new' && (
          <NewTask
            onCreated={(id) => (window.location.hash = `#/task/${id}`)}
            onError={(e) => (e instanceof ApiError && e.status === 401 ? logout() : undefined)}
          />
        )}
        {route.name === 'task' && route.id && <TaskDetails taskId={route.id} />}
      </main>
    </div>
  );
}

export { ApiError };
