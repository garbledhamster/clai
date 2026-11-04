import { expect, it, vi } from 'vitest';
import { render, screen } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { useState } from 'react';
import WeeklyBlockGrid from './WeeklyBlockGrid';

it('selects quarter-hours with pointer and keyboard and moves focus between days', async () => {
  const user = userEvent.setup();
  const changed = vi.fn();
  function Editor() {
    const [slots, setSlots] = useState<number[]>([]);
    return <WeeklyBlockGrid slots={slots} onChange={(next) => { setSlots(next); changed(next); }} />;
  }
  render(<Editor />);
  const first = screen.getByRole('button', { name: 'Monday 00:00–00:15' });
  await user.click(first);
  expect(first).toHaveAttribute('aria-pressed', 'true');
  expect(changed).toHaveBeenLastCalledWith([0]);
  await user.keyboard('{ArrowRight} ');
  expect(screen.getByRole('button', { name: 'Tuesday 00:00–00:15' })).toHaveFocus();
  expect(changed).toHaveBeenLastCalledWith([0, 96]);
  await user.keyboard('{ArrowDown}{Enter}');
  expect(changed).toHaveBeenLastCalledWith([0, 96, 97]);
  await user.click(screen.getByRole('button', { name: 'Clear allowed blocks' }));
  expect(changed).toHaveBeenLastCalledWith([]);
});
