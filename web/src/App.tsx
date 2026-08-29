import { useEffect, useState } from "react";
import { ApiError, isAuthed, logout as apiLogout, markAuthed } from "./api";
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
import Agents from "./components/Agents";
import Conversations from "./components/Conversations";
import SharedContext from "./components/SharedContext";
import McpServers from "./components/McpServers";
import AgentProfiles from "./components/AgentProfiles";
import Admin from "./components/Admin";
import Repositories from "./components/Repositories";
import WorkflowsAuthoring from "./components/WorkflowsAuthoring";

function parseHash(): { name: string; id?: string } {
  const h = window.location.hash.replace(/^#\/?/, "");
  const parts = h.split("/");
  if (parts[0] === "nodes") return { name: "nodes" };
  if (parts[0] === "approvals") return { name: "approvals" };
  if (parts[0] === "audit") return { name: "audit" };
  if (parts[0] === "skills") return { name: "skills" };
  if (parts[0] === "background") return { name: "background" };
  if (parts[0] === "opencode") return { name: "opencode" };
  if (parts[0] === "users") return { name: "users" };
  if (parts[0] === "learnings") return { name: "learnings" };
  if (parts[0] === "agents") return { name: "agents" };
  if (parts[0] === "conversations") return { name: "conversations" };
  if (parts[0] === "context") return { name: "context" };
  if (parts[0] === "mcp") return { name: "mcp" };
  if (parts[0] === "profiles") return { name: "profiles" };
  if (parts[0] === "admin") return { name: "admin" };
  if (parts[0] === "repos") return { name: "repos" };
  if (parts[0] === "authoring") return { name: "authoring" };
  if (parts[0] === "new") return { name: "new" };
  if (parts[0] === "workflows") return { name: "workflows" };
  if (parts[0] === "workflow" && parts[1])
    return { name: "workflow", id: parts[1] };
  if (parts[0] === "task" && parts[1]) return { name: "task", id: parts[1] };
  return { name: "dashboard" };
}

export default function App() {
  const [authed, setAuthed] = useState(isAuthed());
  const [route, setRoute] = useState(parseHash());

  useEffect(() => {
    const onHash = () => setRoute(parseHash());
    window.addEventListener("hashchange", onHash);
    return () => window.removeEventListener("hashchange", onHash);
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
  const cls = (name: string) =>
    "navbtn" + (route.name === name ? " active" : "");

  return (
    <div className="app">
      <header className="topbar">
        <span className="brand">agentgrid</span>
        <nav>
          <button className={cls("dashboard")} onClick={nav("#/")}>
            Dashboard
          </button>
          <button className={cls("nodes")} onClick={nav("#/nodes")}>
            Nodes
          </button>
          <button className={cls("approvals")} onClick={nav("#/approvals")}>
            Approvals
          </button>
          <button className={cls("audit")} onClick={nav("#/audit")}>
            Audit
          </button>
          <button className={cls("skills")} onClick={nav("#/skills")}>
            Skills
          </button>
          <button className={cls("background")} onClick={nav("#/background")}>
            Background
          </button>
          <button className={cls("opencode")} onClick={nav("#/opencode")}>
            Opencode
          </button>
          <button className={cls("workflows")} onClick={nav("#/workflows")}>
            Workflows
          </button>
          <button className={cls("authoring")} onClick={nav("#/authoring")}>
            Workflow authoring
          </button>
          <button className={cls("agents")} onClick={nav("#/agents")}>
            Agents
          </button>
          <button className={cls("profiles")} onClick={nav("#/profiles")}>
            Profiles
          </button>
          <button className={cls("learnings")} onClick={nav("#/learnings")}>
            Learnings
          </button>
          <button
            className={cls("conversations")}
            onClick={nav("#/conversations")}
          >
            Chat
          </button>
          <button className={cls("context")} onClick={nav("#/context")}>
            Context
          </button>
          <button className={cls("mcp")} onClick={nav("#/mcp")}>
            MCP
          </button>
          <button className={cls("users")} onClick={nav("#/users")}>
            Users
          </button>
          <button className={cls("repos")} onClick={nav("#/repos")}>
            Repos
          </button>
          <button className={cls("admin")} onClick={nav("#/admin")}>
            Admin
          </button>
          <button className={cls("new")} onClick={nav("#/new")}>
            New Task
          </button>
        </nav>
        <button className="navbtn logout" onClick={logout}>
          Logout
        </button>
      </header>
      <main className="content">
        {route.name === "dashboard" && (
          <Dashboard onOpen={(id) => (window.location.hash = `#/task/${id}`)} />
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
  );
}
