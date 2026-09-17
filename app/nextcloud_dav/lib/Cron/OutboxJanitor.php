<?php

declare(strict_types=1);

/**
 * SPDX-FileCopyrightText: 2026 nextcloud-dav contributors
 * SPDX-License-Identifier: AGPL-3.0-or-later
 */

namespace OCA\NextcloudDav\Cron;

use OCA\NextcloudDav\AppInfo\Application;
use OCA\NextcloudDav\Outbox\OutboxRepository;
use OCP\AppFramework\Utility\ITimeFactory;
use OCP\BackgroundJob\TimedJob;
use OCP\IConfig;
use Override;
use Psr\Log\LoggerInterface;

/**
 * Retention + safety net for the sidecar outbox.
 *
 * - Deletes `state = 2` (done) rows whose `processed_at` is older than
 *   `dead_letter_keep_days` (default 30).
 * - Re-arms stale `state = 1` reservations (a worker that died without
 *   releasing its claim). This is belt and braces next to the worker's own
 *   stale-claim handling in `occ dav:event-dispatch`.
 *
 * `attempts` is intentionally left untouched when re-arming: a worker crash is
 * not a poison event, and incrementing here would let a crash-looping worker
 * dead-letter perfectly good events.
 */
class OutboxJanitor extends TimedJob {
	private const DEFAULT_DEAD_LETTER_KEEP_DAYS = 30;
	private const DEFAULT_CLAIM_TIMEOUT_S = 300;

	public function __construct(
		ITimeFactory $timeFactory,
		private readonly OutboxRepository $repository,
		private readonly IConfig $config,
		private readonly LoggerInterface $logger,
	) {
		parent::__construct($timeFactory);
		$this->setInterval(10 * 60);
		$this->setTimeSensitivity(self::TIME_INSENSITIVE);
	}

	#[\Override]
	public function run($argument): void {
		$dispatch = $this->config->getSystemValue(Application::CONFIG_KEY, []);
		$dispatch = is_array($dispatch) ? ($dispatch[Application::CONFIG_DISPATCH_KEY] ?? []) : [];
		$dispatch = is_array($dispatch) ? $dispatch : [];

		$keepDays = max(1, (int)($dispatch['dead_letter_keep_days'] ?? self::DEFAULT_DEAD_LETTER_KEEP_DAYS));
		$claimTimeout = max(1, (int)($dispatch['claim_timeout_s'] ?? self::DEFAULT_CLAIM_TIMEOUT_S));

		$now = time();
		$reaped = $this->repository->reapStaleReservations($now - $claimTimeout);
		$deleted = $this->repository->deleteDoneBefore($now - $keepDays * 86400);

		if ($reaped > 0 || $deleted > 0) {
			$this->logger->info('nextcloud_dav: outbox janitor re-armed {reaped} stale reservations and deleted {deleted} done rows', [
				'app' => 'nextcloud_dav',
				'reaped' => $reaped,
				'deleted' => $deleted,
			]);
		}
	}
}