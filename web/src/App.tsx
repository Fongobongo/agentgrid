import { useEffect, useState } from 'react';
import { ApiError, isAuthed, logout as apiLogout, markAuthed } from './api';
import Login from './components/Login';
import Dashboard from './components/Dashboard';
import Nodes from './components/Nodes';
import NewTask from './components/NewTask';
import TaskDetails from './components/TaskDetails';
import Approvals from './components/Approvals';
import Audit from './components/Audit';
import Skills from './components/Skills';
import { WorkflowsList, WorkflowDetails } from './components/Workflows';

function parseHash(): { name: string; id?: string } {
  const h = window.location.hash.replace(/^#\/?/, '');
  const parts = h.split('/');
  if (parts[0] === 'nodes') return { name: 'nodes' };
  if (parts[0] === 'approvals') return { name: 'approvals' };
  if (parts[0] === 'audit') return { name: 'audit' };
  if (parts[0] === 'skills') return { name: 'skills' };
  if (parts[0] === 'new') return { name: 'new' };
  if (parts[0] === 'workflows') return { name: 'workflows' };
  if (parts[0] === 'workflow' && parts[1]) return { name: 'workflow', id: parts[1] };
  if (parts[0] === 'task' && parts[1]) return { name: 'task', id: parts[1] };
  return { name: 'dashboard' };
}

export default function App() {
  const [authed, setAuthed] = useState(isAuthed());
  const [route, setRoute] = useState(parseHash());

  useEffect(() => {
    const onHash = () => setRoute(parseHash());
    window.addEventListener('hashchange', onHash);
    return () => window.removeEventListener('hashchange', onHash);
  }, []);

  const logout = async () => {
    await apiLogout();
    setAuthed(false);
  };

  const onAuthed = () => {
    markAuthed();
    setAuthed(true);
    window.location.hash = '#/';
  };

  if (!authed) return <Login onAuthed={onAuthed} />;

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
          <button className={cls('approvals')} onClick={nav('#/approvals')}>Approvals</button>
          <button className={cls('audit')} onClick={nav('#/audit')}>Audit</button>
          <button className={cls('skills')} onClick={nav('#/skills')}>Skills</button>
          <button className={cls('workflows')} onClick={nav('#/workflows')}>Workflows</button>
          <button className={cls('new')} onClick={nav('#/new')}>New Task</button>
        </nav>
        <button className="navbtn logout" onClick={logout}>Logout</button>
      </header>
      <main className="content">
        {route.name === 'dashboard' && <Dashboard onOpen={(id) => (window.location.hash = `#/task/${id}`)} />}
        {route.name === 'nodes' && <Nodes />}
        {route.name === 'approvals' && <Approvals />}
        {route.name === 'audit' && <Audit />}
        {route.name === 'skills' && <Skills />}
        {route.name === 'workflows' && <WorkflowsList onOpen={(id) => (window.location.hash = `#/workflow/${id}`)} />}
        {route.name === 'workflow' && route.id && <WorkflowDetails key={route.id} runId={route.id} />}
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
