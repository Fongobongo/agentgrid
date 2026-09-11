import { useEffect, useState } from "react";
import { ApiError, countPendingApprovals, isAuthed, logout as apiLogout, markAuthed, streamChanges } from "./api";
import Login from "./components/Login";
import Dashboard from "./components/Dashboard";
import Nodes from "./components/Nodes";
import NewTask from "./components/NewTask";
import TaskDetails from "./components/TaskDetails";
import Approvals from "./components/Approvals";
import Audit from "./components/Audit";
import Skills from "./components/Skills";
import { WorkflowsList, WorkflowDetails } from "./components/Workflows";
import Background from "./components/Background";
import OpencodeProfiles from "./components/OpencodeProfiles";
import Users from "./components/Users";
import Learnings from "./components/Learnings";
import Proxies from "./components/Proxies";
import Agents from "./components/Agents";
import Conversations from "./components/Conversations";
import SharedContext from "./components/SharedContext";
import McpServers from "./components/McpServers";
import AgentProfiles from "./components/AgentProfiles";
import Admin from "./components/Admin";
import Repositories from "./components/Repositories";
import WorkflowsAuthoring from "./components/WorkflowsAuthoring";
import { ToastHost } from "./components/Toast";
import { Theme, applyTheme, setTheme, storedTheme } from "./theme";

interface Route {
  name: string;
  id?: string;
}

function parseHash(): { name: string; id?: string; unknown?: boolean } {
  const h = window.location.hash.replace(/^#\/?/, "");
  const parts = h.split("/");
  if (parts[0] === "") return { name: "dashboard" };
  const routes = [
    "nodes",
    "approvals",
    "audit",
    "skills",
    "background",
    "opencode",
    "users",
    "learnings",
    "agents",
    "conversations",
    "context",
    "mcp",
    "profiles",
    "admin",
    "repos",
    "proxies",
    "authoring",
    "new",
    "workflows",
  ];
  if (routes.includes(parts[0])) return { name: parts[0] };
  if (parts[0] === "workflow" && parts[1])
    return { name: "workflow", id: parts[1] };
  if (parts[0] === "task" && parts[1]) return { name: "task", id: parts[1] };
  return { name: "dashboard", unknown: true };
}

interface NavItem {
  label: string;
  route: Route;
  hash: string;
  accent?: boolean;
}

interface NavGroup {
  title: string;
  items: NavItem[];
}

// Sidebar groups keep 20+ destinations scannable instead of one
// overflowing topbar row. `accent` marks the primary action (New Task).
const NAV_GROUPS: NavGroup[] = [
  {
    title: "Overview",
    items: [
      { label: "Dashboard", route: { name: "dashboard" }, hash: "#/" },
      { label: "Nodes", route: { name: "nodes" }, hash: "#/nodes" },
      { label: "Approvals", route: { name: "approvals" }, hash: "#/approvals" },
    ],
  },
  {
    title: "Tasks",
    items: [
      { label: "New Task", route: { name: "new" }, hash: "#/new", accent: true },
      { label: "Background", route: { name: "background" }, hash: "#/background" },
      { label: "Chat", route: { name: "conversations" }, hash: "#/conversations" },
      { label: "Context", route: { name: "context" }, hash: "#/context" },
    ],
  },
  {
    title: "Workflows",
    items: [
      { label: "Runs", route: { name: "workflows" }, hash: "#/workflows" },
      {
        label: "Authoring",
        route: { name: "authoring" },
        hash: "#/authoring",
      },
    ],
  },
  {
    title: "Agents & Skills",
    items: [
      { label: "Agents", route: { name: "agents" }, hash: "#/agents" },
      { label: "Profiles", route: { name: "profiles" }, hash: "#/profiles" },
      { label: "Skills", route: { name: "skills" }, hash: "#/skills" },
      { label: "Learnings", route: { name: "learnings" }, hash: "#/learnings" },
      { label: "Opencode", route: { name: "opencode" }, hash: "#/opencode" },
    ],
  },
  {
    title: "Platform",
    items: [
      { label: "MCP", route: { name: "mcp" }, hash: "#/mcp" },
      { label: "Repos", route: { name: "repos" }, hash: "#/repos" },
      { label: "Proxies", route: { name: "proxies" }, hash: "#/proxies" },
      { label: "Audit", route: { name: "audit" }, hash: "#/audit" },
    ],
  },
  {
    title: "Admin",
    items: [
      { label: "Users", route: { name: "users" }, hash: "#/users" },
      { label: "Admin", route: { name: "admin" }, hash: "#/admin" },
    ],
  },
];

function sameRoute(a: Route, b: Route): boolean {
  return a.name === b.name && a.id === b.id;
}

// Human-readable page name per route for document.title. Detail pages get
// a short id suffix so browser history / tabs stay distinguishable.
const ROUTE_TITLES: Record<string, string> = {
  dashboard: "Dashboard",
  nodes: "Nodes",
  approvals: "Approvals",
  audit: "Audit",
  skills: "Skills",
  background: "Background",
  opencode: "Opencode",
  users: "Users",
  learnings: "Learnings",
  agents: "Agents",
  conversations: "Chat",
  context: "Context",
  mcp: "MCP",
  profiles: "Profiles",
  admin: "Admin",
  repos: "Repos",
  proxies: "Proxies",
  authoring: "Workflow authoring",
  new: "New Task",
  workflows: "Workflow runs",
  workflow: "Workflow run",
  task: "Task",
};

export default function App() {
  const [authed, setAuthed] = useState(isAuthed());
  const [route, setRoute] = useState(parseHash);
  const [menuOpen, setMenuOpen] = useState(false);
  const [theme, setThemeState] = useState<Theme>(storedTheme);
  // Sidebar badge: pending approval count, refreshed on the change stream.
  const [pendingCount, setPendingCount] = useState<number | null>(null);

  useEffect(() => {
    applyTheme(theme); // keep <html data-theme> in sync on toggle
  }, [theme]);

  // Reflect the current page in document.title ("agentgrid — Nodes").
  useEffect(() => {
    const base = ROUTE_TITLES[route.name] ?? "Dashboard";
    document.title = `agentgrid — ${base}${route.id ? ` ${route.id.slice(0, 8)}` : ""}`;
  }, [route]);

  // Pending-approvals badge: fetch once on auth, then re-check whenever
  // the control-plane change stream reports movement.
  const refreshPending = () => {
    countPendingApprovals().then(setPendingCount);
  };
  useEffect(() => {
    if (!authed) return;
    refreshPending();
    const h = streamChanges(refreshPending);
    return () => h.close();
  }, [authed]);

  useEffect(() => {
    const onHash = () => {
      setRoute(parseHash());
      setMenuOpen(false);
    };
    window.addEventListener("hashchange", onHash);
    return () => window.removeEventListener("hashchange", onHash);
  }, []);

  // Close the mobile menu on Escape.
  useEffect(() => {
    if (!menuOpen) return;
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") setMenuOpen(false);
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [menuOpen]);

  // Hotkeys: `n` -> New Task, `/` -> focus the dashboard search box.
  // Both no-op while typing in an input/textarea/select.
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      const el = e.target as HTMLElement | null;
      if (
        el &&
        (el.tagName === "INPUT" ||
          el.tagName === "TEXTAREA" ||
          el.tagName === "SELECT" ||
          el.isContentEditable)
      )
        return;
      if (e.key === "n" || e.key === "N") {
        if (!e.ctrlKey && !e.metaKey && !e.altKey) {
          e.preventDefault();
          window.location.hash = "#/new";
        }
      } else if (e.key === "/") {
        if (window.location.hash.replace(/^#\/?/, "").split("/")[0] === "") {
          e.preventDefault();
          document
            .querySelector<HTMLInputElement>(".search-input")
            ?.focus();
        }
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, []);

  const logout = async () => {
    await apiLogout();
    setAuthed(false);
  };

  const onAuthed = () => {
    markAuthed();
    setAuthed(true);
    window.location.hash = "#/";
  };

  if (!authed) return <Login onAuthed={onAuthed} />;

  const nav = (to: string) => () => {
    window.location.hash = to;
  };

  const sidebar = (
    <nav className="sidenav" aria-label="Main navigation">
      {NAV_GROUPS.map((g) => (
        <div className="navgroup" key={g.title}>
          <div className="navgroup-title">{g.title}</div>
          {g.items.map((item) => {
            const active = sameRoute(route, item.route);
            const badge =
              item.route.name === "approvals" && pendingCount
                ? pendingCount
                : null;
            return (
              <button
                className={"navbtn" + (active ? " active" : "") + (item.accent ? " accent" : "")}
                aria-current={active ? "page" : undefined}
                onClick={nav(item.hash)}
                key={item.hash}
              >
                <span className="navbtn-inner">
                  {item.label}
                  {badge !== null && (
                    <span
                      className={
                        "navbadge" + (badge > 0 ? " hot" : "")
                      }
                      aria-label={`${badge} pending approvals`}
                    >
                      {badge > 99 ? "99+" : badge}
                    </span>
                  )}
                </span>
              </button>
            );
          })}
        </div>
      ))}
      <button className="navbtn logout" onClick={logout}>
        Logout
      </button>
    </nav>
  );

  return (
    <div className="app">
      <header className="topbar">
        <button
          className="menu-toggle"
          aria-label="Toggle navigation menu"
          aria-expanded={menuOpen}
          onClick={() => setMenuOpen(!menuOpen)}
        >
          ☰
        </button>
        <span className="brand">agentgrid</span>
        <button
          className="topbar-new"
          onClick={() => (window.location.hash = "#/new")}
          title="New task (shortcut: n)"
        >
          + New Task
        </button>
        <button
          className="theme-toggle"
          aria-label={
            theme === 'dark' ? 'Switch to light theme' : 'Switch to dark theme'
          }
          title="Toggle theme"
          onClick={() => {
            const next = theme === 'dark' ? 'light' : 'dark';
            setTheme(next);
            setThemeState(next);
          }}
        >
          {theme === 'dark' ? '☀' : '☾'}
        </button>
      </header>
      {menuOpen && (
        <div
          className="scrim"
          role="presentation"
          onClick={() => setMenuOpen(false)}
        />
      )}
      <div className={"app-body" + (menuOpen ? " menu-open" : "")}>
        <aside className={"sidebar" + (menuOpen ? " open" : "")}>{sidebar}</aside>
        <main className="content">
          {route.unknown && (
            <div className="error">
              Page not found: #{window.location.hash}. Showing the dashboard
              instead.
            </div>
          )}
          {route.name === "dashboard" && (
            <Dashboard
              onOpen={(id) => (window.location.hash = `#/task/${id}`)}
            />
          )}
          {route.name === "nodes" && <Nodes />}
          {route.name === "approvals" && <Approvals />}
          {route.name === "audit" && <Audit />}
          {route.name === "skills" && <Skills />}
          {route.name === "background" && <Background />}
          {route.name === "opencode" && <OpencodeProfiles />}
          {route.name === "workflows" && (
            <WorkflowsList
              onOpen={(id) => (window.location.hash = `#/workflow/${id}`)}
            />
          )}
          {route.name === "authoring" && (
            <WorkflowsAuthoring
              onCreated={(id) => (window.location.hash = `#/workflow/${id}`)}
            />
          )}
          {route.name === "agents" && <Agents />}
          {route.name === "profiles" && <AgentProfiles />}
          {route.name === "learnings" && <Learnings />}
          {route.name === "conversations" && <Conversations />}
          {route.name === "context" && <SharedContext />}
          {route.name === "mcp" && <McpServers />}
          {route.name === "users" && <Users />}
          {route.name === "repos" && <Repositories />}
          {route.name === "proxies" && <Proxies />}
          {route.name === "admin" && <Admin />}
          {route.name === "workflow" && route.id && (
            <WorkflowDetails key={route.id} runId={route.id} />
          )}
          {route.name === "new" && (
            <NewTask
              onCreated={(id) => (window.location.hash = `#/task/${id}`)}
              onError={(e) =>
                e instanceof ApiError && e.status === 401 ? logout() : undefined
              }
            />
          )}
          {route.name === "task" && route.id && <TaskDetails taskId={route.id} />}
        </main>
      </div>
      <ToastHost />
    </div>
  );
}
