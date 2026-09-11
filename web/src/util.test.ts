import { describe, it, expect } from 'vitest';
import { fmtAgo, fmtTime } from './components/util';

// fmtAgo is pure (now injected), so these run without DOM setup.

describe('fmtAgo', () => {
  const now = Date.parse('2026-09-12T12:00:00Z');
  const iso = (secondsAgo: number) =>
    new Date(now - secondsAgo * 1000).toISOString();

  it('returns an em dash for null/empty (unknown time)', () => {
    expect(fmtAgo(null, now)).toBe('—');
    expect(fmtAgo('', now)).toBe('—');
  });

  it('passes malformed values through untouched', () => {
    expect(fmtAgo('not-a-date', now)).toBe('not-a-date');
  });

  it('formats fresh moments as "just now"', () => {
    expect(fmtAgo(iso(2), now)).toBe('just now');
    expect(fmtAgo(iso(0), now)).toBe('just now');
  });

  it('formats seconds, minutes and hours', () => {
    expect(fmtAgo(iso(45), now)).toBe('45s ago');
    expect(fmtAgo(iso(90), now)).toBe('2m ago');
    expect(fmtAgo(iso(25 * 60), now)).toBe('25m ago');
    expect(fmtAgo(iso(3 * 3600), now)).toBe('3h ago');
    expect(fmtAgo(iso(23 * 3600), now)).toBe('23h ago');
  });

  it('formats days and falls back to a date past a month', () => {
    expect(fmtAgo(iso(2 * 86400), now)).toBe('2d ago');
    expect(fmtAgo(iso(29 * 86400), now)).toBe('29d ago');
    // 30+ days renders as a locale date; assert it is NOT a relative form.
    const old = fmtAgo(iso(40 * 86400), now);
    expect(old).not.toMatch(/^\d+[smhd] ago$/);
    expect(old).toMatch(/\d{4}/); // some year-bearing format
  });

  it('never renders negative ages for future timestamps', () => {
    const future = new Date(now + 30_000).toISOString();
    expect(fmtAgo(future, now)).toBe('just now');
  });
});

describe('fmtTime', () => {
  it('returns an em dash for null and passes bad values through', () => {
    expect(fmtTime(null)).toBe('—');
    expect(fmtTime('nope')).toBe('nope');
  });

  it('parses valid ISO timestamps', () => {
    expect(fmtTime('2026-09-12T12:00:00Z')).toMatch(/2026/);
  });
});
