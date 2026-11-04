import { useRef, useState } from 'react';
import styles from './WeeklyBlockGrid.module.css';

const DAYS = ['Monday', 'Tuesday', 'Wednesday', 'Thursday', 'Friday', 'Saturday', 'Sunday'];
const time = (quarter: number) => `${String(Math.floor(quarter / 4)).padStart(2, '0')}:${String((quarter % 4) * 15).padStart(2, '0')}`;

export default function WeeklyBlockGrid({ slots, onChange, disabled = false }: {
  slots: number[];
  onChange: (slots: number[]) => void;
  disabled?: boolean;
}) {
  const [focus, setFocus] = useState(0);
  const buttons = useRef(new Map<number, HTMLButtonElement>());
  const selected = new Set(slots);
  const toggle = (slot: number) => {
    const next = new Set(slots);
    if (next.has(slot)) next.delete(slot); else next.add(slot);
    onChange([...next].sort((a, b) => a - b));
  };
  return (
    <div>
      <p id="weekly-block-help" className={styles.help}>Select allowed quarter-hours. Use arrow keys to move, Space or Enter to toggle. No selected blocks means unrestricted scheduling.</p>
      <div className={styles.scroll} role="group" aria-label="Weekly allowed blocks" aria-describedby="weekly-block-help">
        <div className={styles.grid}>
          <span />{DAYS.map((day) => <strong className={styles.day} key={day}>{day.slice(0, 3)}</strong>)}
          {Array.from({ length: 96 }, (_, quarter) => (
            <div className={styles.row} key={quarter}>
              <span className={styles.time}>{time(quarter)}</span>
              {DAYS.map((day, dayIndex) => {
                const slot = dayIndex * 96 + quarter;
                return <button key={slot} type="button" disabled={disabled}
                  ref={(element) => { if (element) buttons.current.set(slot, element); else buttons.current.delete(slot); }}
                  className={styles.cell} aria-label={`${day} ${time(quarter)}–${time(quarter + 1)}`}
                  aria-pressed={selected.has(slot)} tabIndex={slot === focus ? 0 : -1}
                  onFocus={() => setFocus(slot)} onClick={() => toggle(slot)}
                  onKeyDown={(event) => {
                    const day = Math.floor(slot / 96);
                    const q = slot % 96;
                    const next = event.key === 'ArrowRight' ? ((day + 1) % 7) * 96 + q
                      : event.key === 'ArrowLeft' ? ((day + 6) % 7) * 96 + q
                      : event.key === 'ArrowDown' ? day * 96 + (q + 1) % 96
                      : event.key === 'ArrowUp' ? day * 96 + (q + 95) % 96 : null;
                    if (next !== null) { event.preventDefault(); buttons.current.get(next)?.focus(); }
                  }}
                >{selected.has(slot) ? '✓' : ''}</button>;
              })}
            </div>
          ))}
        </div>
      </div>
      <div className={styles.footer}>
        <span aria-live="polite">{slots.length} of 672 blocks selected</span>
        <button type="button" disabled={disabled || slots.length === 0} onClick={() => onChange([])}>Clear allowed blocks</button>
      </div>
    </div>
  );
}
