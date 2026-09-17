<?php

declare(strict_types=1);

/**
 * SPDX-FileCopyrightText: 2026 nextcloud-dav contributors
 * SPDX-License-Identifier: AGPL-3.0-or-later
 */

namespace OCA\NextcloudDav\Settings;

use OCA\NextcloudDav\AppInfo\Application;
use OCA\NextcloudDav\Outbox\OutboxRepository;
use OCP\AppFramework\Http\TemplateResponse;
use OCP\Settings\ISettings;
use Override;

/**
 * Read-only admin panel showing the outbox backlog next to the other DAV
 * (groupware) settings. It is observability only; the worker and the janitor
 * are the actors.
 */
class Admin implements ISettings {
	public function __construct(
		private readonly OutboxRepository $repository,
	) {
	}

	#[\Override]
	public function getForm(): TemplateResponse {
		$counts = $this->repository->counts();
		$oldestAge = $counts['oldest_pending_at'] !== null ? max(0, time() - $counts['oldest_pending_at']) : 0;

		return new TemplateResponse(Application::APP_ID, 'settings-admin', [
			'pending' => $counts['pending'],
			'claimed' => $counts['claimed'],
			'dead' => $counts['dead'],
			'oldestPendingAge' => $oldestAge,
		], '');
	}

	#[\Override]
	public function getSection(): string {
		return 'groupware';
	}

	#[\Override]
	public function getPriority(): int {
		return 90;
	}
}