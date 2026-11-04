import { describe, expect, it } from 'vitest';
import { formatScheduleLabel } from './workspaceStatus';

describe('formatScheduleLabel', () => {
  it('describes enhanced second intervals and cron schedules', () => {
    expect(formatScheduleLabel({ type: 'enhancedInterval', intervalSeconds: 15 })).toBe('every 15s');
    expect(formatScheduleLabel({ type: 'enhancedCron', expression: '0 9 * * *', timezone: 'UTC' })).toBe('cron: 0 9 * * *');
  });
});
