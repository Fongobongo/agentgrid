import { describe, it, expect } from 'vitest';
import { buildAssemblyBody } from './components/Assembly';

describe('buildAssemblyBody', () => {
  const base = {
    goal: 'build a blog',
    adapter: '',
    repository: '',
    autoApprove: false,
    githubPush: false,
    githubRepo: '',
    githubIssue: '',
    githubCreate: false,
  };

  it('sends the goal alone by default (server defaults apply)', () => {
    expect(buildAssemblyBody(base)).toEqual({ goal: 'build a blog' });
  });

  it('passes autopilot + publish flags through', () => {
    const b = buildAssemblyBody({
      ...base,
      adapter: 'claude',
      repository: 'demo',
      autoApprove: true,
      githubPush: true,
      githubRepo: 'acme/blog',
      githubIssue: '7',
      githubCreate: true,
    });
    expect(b).toEqual({
      goal: 'build a blog',
      adapter: 'claude',
      repository: 'demo',
      auto_approve_plan: true,
      github_push: true,
      github_repo: 'acme/blog',
      github_issue: 7,
      github_create: true,
    });
  });

  it('drops blank optionals', () => {
    const b = buildAssemblyBody({ ...base, adapter: '  ', githubRepo: ' ' });
    expect(b).toEqual({ goal: 'build a blog' });
  });
});
