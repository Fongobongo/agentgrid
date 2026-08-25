import { useEffect, useState } from 'react';
import { listSkills, reqOk, setSkillTrust, SkillTrustView } from '../api';
import { ConfirmModal } from './Modal';
import { ErrorBox, Loading, fmtTime, useLiveRefresh } from './util';

// Stage 9.2 skill trust ledger: list recorded trust decisions and flip a
// skill between trusted/untrusted. Fail-closed: a skill absent from the
// ledger is untrusted (the agent may not load/execute it). Refreshes on the
// control-plane change stream; node-side skill discovery wiring is a
// follow-up — for now the table shows whatever the operator has decided
// plus anything a future node report back-fills.

export default function Skills() {
  const [items, setItems] = useState<SkillTrustView[] | null>(null);
  const [error, setError] = useState<unknown>(null);
  const [busy, setBusy] = useState<string | null>(null);
  const [confirming, setConfirming] = useState<SkillTrustView | null>(null);

  const load = () => {
    listSkills()
      .then((items) => {
        setError(null);
        setItems(items);
      })
      .catch(setError);
  };
  useEffect(load, []);
  useLiveRefresh(load);

  const toggle = async (s: SkillTrustView) => {
    const next = !s.trusted;
    setBusy(`${s.name}/${s.source}`);
    try {
      await reqOk(await setSkillTrust(s.name, s.source, next));
      load();
    } catch (e) {
      setError(e);
    } finally {
      setBusy(null);
    }
  };

  if (!items) {
    if (error) return <ErrorBox err={error} />;
    return <Loading />;
  }

  return (
    <section>
      <h2>Skills — trust</h2>
      {error ? <ErrorBox err={error} /> : null}
      <div className="muted">
        A skill not listed here is <b>untrusted</b> by default (fail-closed):
        the agent may not load or execute it until you trust it.
      </div>
      {items.length === 0 ? (
        <div className="muted">No recorded trust decisions yet.</div>
      ) : (
        <table className="grid">
          <thead>
            <tr>
              <th>Trusted</th>
              <th>Name</th>
              <th>Source</th>
              <th>Decided by</th>
              <th>Decided at</th>
              <th></th>
            </tr>
          </thead>
          <tbody>
            {items.map((s) => (
              <tr key={`${s.name}/${s.source}`}>
                <td>{s.trusted ? '✅' : '⛔'}</td>
                <td className="mono">{s.name}</td>
                <td>{s.source}</td>
                <td>{s.decided_by || '—'}</td>
                <td>{fmtTime(s.decided_at ?? null)}</td>
                <td>
                  <button
                    className={s.trusted ? 'danger' : 'ok'}
                    disabled={busy === `${s.name}/${s.source}`}
                    onClick={() => setConfirming(s)}
                  >
                    {s.trusted ? 'Untrust' : 'Trust'}
                  </button>
                </td>
              </tr>
            ))}
          </tbody>
        </table>
      )}
      {confirming && (
        <ConfirmModal
          title={`${confirming.trusted ? 'Untrust' : 'Trust'} skill`}
          body={`${confirming.trusted ? 'Untrust' : 'Trust'} skill "${confirming.name}" (${confirming.source})?`}
          confirmLabel={confirming.trusted ? 'Untrust' : 'Trust'}
          danger={confirming.trusted}
          onConfirm={() => {
            const s = confirming;
            setConfirming(null);
            toggle(s);
          }}
          onCancel={() => setConfirming(null)}
        />
      )}
    </section>
  );
}
