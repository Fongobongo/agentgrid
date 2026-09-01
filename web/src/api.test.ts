import { describe, it, expect, vi, afterEach } from 'vitest';
import { ApiError, reqOk, getJson, postJson } from './api';

// Response/Request are global in the vitest node environment (Node 22).

describe('reqOk', () => {
  it('passes an OK response through untouched', async () => {
    const r = new Response('{"a":1}', { status: 200 });
    await expect(reqOk(r)).resolves.toBe(r);
  });

  it('throws ApiError carrying the status for non-OK', async () => {
    vi.stubGlobal('location', { origin: 'http://localhost' });
    const r = new Response('nope', { status: 409 });
    const e = await reqOk(r).catch((e) => e);
    expect(e).toBeInstanceOf(ApiError);
    expect(e.status).toBe(409);
    expect(e.message).toContain('409');
  });
});

describe('getJson / postJson', () => {
  afterEach(() => vi.unstubAllGlobals());

  it('getJson parses the body of a 200', async () => {
    vi.stubGlobal('fetch', vi.fn(async () => new Response('{"id":"t1"}', { status: 200 })));
    await expect(getJson<{ id: string }>('/v1/tasks/t1')).resolves.toEqual({ id: 't1' });
  });

  it('getJson throws ApiError on 404 (no body parse)', async () => {
    vi.stubGlobal('fetch', vi.fn(async () => new Response('', { status: 404 })));
    const e = await getJson('/v1/tasks/nope').catch((e) => e);
    expect(e).toBeInstanceOf(ApiError);
    expect(e.status).toBe(404);
  });

  it('postJson sends JSON with content-type and parses the reply', async () => {
    const f = vi.fn(async () => new Response('{"ok":true}', { status: 201 }));
    vi.stubGlobal('fetch', f);
    await expect(postJson<{ ok: boolean }>('/v1/tasks', { prompt: 'x' })).resolves.toEqual({ ok: true });
    const init = f.mock.calls[0][1] as RequestInit;
    expect(init.method).toBe('POST');
    expect((init.headers as Record<string, string>)['Content-Type']).toBe('application/json');
    expect(init.body).toBe(JSON.stringify({ prompt: 'x' }));
  });
});
