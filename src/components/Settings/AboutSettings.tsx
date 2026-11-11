/**
 * AboutSettings Component
 *
 * The CLAI+ fork introduction and desktop update controls.
 */

import React, { useEffect, useState } from 'react';
import { invoke } from '@tauri-apps/api/core';
import type { AppUpdateCheckResult, AppUpdateStatus } from '../../generated/bindings';
import {
  LATEST_RELEASE_URL,
  confirmRestartWithLiveRuns,
  installAppUpdate,
  installEventText,
  updateErrorText,
} from '../../utils/appUpdates';
import { openExternal } from '../../utils/openExternal';
import styles from './AboutSettings.module.css';

const ABOUT_URL = 'https://garbledhamster.github.io/clai/';

const AboutSettings = () => {
  const [version, setVersion] = useState('');
  const [updateStatus, setUpdateStatus] = useState<AppUpdateStatus | null>(null);
  const [updateError, setUpdateError] = useState('');
  const [checking, setChecking] = useState(false);
  const [installing, setInstalling] = useState(false);
  // Set while asking about live runs, so that a second click cannot ask again.
  const [confirming, setConfirming] = useState(false);
  const [installProgress, setInstallProgress] = useState('');

  useEffect(() => {
    let cancelled = false;
    // Dev-aware version: the crate version in a release build, or the
    // `git describe` string (e.g. `26.6.7-38-g6148106`) in a dev build past
    // the last release tag. See commands::app_info::app_version_detail.
    invoke<string>('app_version_detail')
      .then((value) => {
        if (!cancelled) setVersion(value);
      })
      .catch((err) => {
        // Version is non-essential; leave it blank rather than failing the page.
        console.error('[AboutSettings] Failed to read app version:', err);
      });
    return () => {
      cancelled = true;
    };
  }, []);

  useEffect(() => {
    let cancelled = false;
    invoke<AppUpdateStatus>('get_app_update_status')
      .then((status) => {
        if (!cancelled) setUpdateStatus(status);
      })
      .catch((err) => {
        if (!cancelled) setUpdateError(updateErrorText(err, 'Failed to read update status.'));
      });
    return () => {
      cancelled = true;
    };
  }, []);

  const checkForUpdates = async () => {
    setChecking(true);
    setUpdateError('');
    try {
      const result = await invoke<AppUpdateCheckResult>('check_for_app_update');
      setUpdateStatus({ support: result.support, lastCheck: result.lastCheck });
    } catch (err) {
      setUpdateError(updateErrorText(err, 'Failed to check for updates.'));
    } finally {
      setChecking(false);
    }
  };

  const installUpdate = async () => {
    setUpdateError('');
    setConfirming(true);
    try {
      if (!(await confirmRestartWithLiveRuns())) return;
      setInstalling(true);
      setInstallProgress('Starting download...');
      await installAppUpdate((event) => {
        setInstallProgress(installEventText(event));
      });
    } catch (err) {
      setUpdateError(updateErrorText(err, 'Failed to install update.'));
      setInstalling(false);
    } finally {
      setConfirming(false);
    }
  };

  const support = updateStatus?.support;
  const lastCheck = updateStatus?.lastCheck;
  const availableUpdate = lastCheck?.update ?? null;
  const supportsUpdates = support?.supported ?? false;
  const canCheck = support?.canCheck ?? false;
  const isLinux = support?.platform === 'linux';
  const supportDescription = updateStatus
    ? support?.reason || `${support?.platform ?? 'Desktop'} ${support?.bundleType ?? 'native'}`
    : 'Checking update support...';
  const supportBadge = !updateStatus
    ? 'Checking'
    : supportsUpdates
      ? 'Available'
      : canCheck
        ? 'Notify only'
        : 'Unavailable';
  // Builds that can neither self-update nor check (dev builds,
  // package-manager installs like AUR) render no Updates panel at all:
  // updates arrive through the build/package channel, so there is nothing
  // actionable to show. The panel also stays hidden while support is still
  // loading — appearing once is fine, appearing and then vanishing is not.
  // If the status read itself fails, the panel shows so the error is visible.
  const updatesUnavailable = updateStatus !== null && !supportsUpdates && !canCheck;
  const showUpdatePanel = updateError !== '' || (updateStatus !== null && !updatesUnavailable);

  // When the new version isn't installable from within CLAI itself, the
  // summary line needs platform-aware copy: macOS/Windows users fall back
  // to the manual "Install and restart" / GitHub download, while Linux
  // builds are steward-managed by the user's package manager or Flatpak
  // (notify-only by design).
  const showPackageManagerHint =
    availableUpdate !== null && !availableUpdate.installable && isLinux;
  const updateSummary = installing
    ? installProgress
    : availableUpdate
      ? availableUpdate.downloaded
        ? `CLAI v${availableUpdate.version} is downloaded and ready to install.`
        : showPackageManagerHint
          ? `CLAI v${availableUpdate.version} is available. Use your package manager to install it.`
          : `CLAI v${availableUpdate.version} is available.`
      : lastCheck?.error || (lastCheck ? 'CLAI is up to date.' : 'Not checked yet.');

  return (
    <div className={styles.hero}>
      <div className={styles.forkIntro}>
        <h3 className={styles.title}>
          CLAI<span>+</span>
        </h3>
        {version && <span className={styles.version}>v{version}</span>}
        <p className={styles.subtitle}>My fork of CLAI for running agent teams on a schedule.</p>
        <ul className={styles.features}>
          <li>🗓️ Workspace schedules with weekly time blocks</li>
          <li>⚙️ Condition, Before, and After scripts</li>
        </ul>
        <button
          type="button"
          className={styles.aboutButton}
          onClick={() => {
            openExternal(ABOUT_URL).catch((err) =>
              console.error('[AboutSettings] Failed to open link:', err)
            );
          }}
        >
          <span>About the fork and screenshots</span>
          <span aria-hidden="true">↗</span>
        </button>
      </div>

      {showUpdatePanel && (
        <div className={styles.updatePanel}>
          <div className={styles.updateHeader}>
            <div className={styles.updateTitleGroup}>
              <span className={styles.updateTitle}>Updates</span>
              <span className={styles.updateDesc}>{supportDescription}</span>
            </div>
            <span
              className={`${styles.updateBadge} ${supportsUpdates ? styles.updateBadgeOk : ''}`}
            >
              {supportBadge}
            </span>
          </div>

          <div className={styles.updateStatus}>
            <span>{updateSummary}</span>
            {updateError && <span className={styles.updateError}>{updateError}</span>}
          </div>

          <div className={styles.updateActions}>
            <button
              type="button"
              className={styles.secondaryButton}
              onClick={checkForUpdates}
              disabled={checking || !canCheck}
            >
              {checking ? 'Checking...' : 'Check for updates'}
            </button>
            {availableUpdate &&
              (availableUpdate.installable ? (
                <button
                  type="button"
                  className={styles.primaryButton}
                  onClick={installUpdate}
                  disabled={installing || confirming}
                >
                  {installing
                    ? 'Installing...'
                    : availableUpdate.downloaded
                      ? 'Restart and update'
                      : 'Install and restart'}
                </button>
              ) : (
                <button
                  type="button"
                  className={styles.primaryButton}
                  onClick={() => {
                    openExternal(LATEST_RELEASE_URL).catch((err) =>
                      console.error('[AboutSettings] Failed to open release page:', err)
                    );
                  }}
                >
                  {showPackageManagerHint ? 'Open release notes' : 'View release'}
                </button>
              ))}
          </div>
        </div>
      )}
    </div>
  );
};

export default AboutSettings;
