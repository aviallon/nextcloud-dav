<?php

declare(strict_types=1);

/**
 * SPDX-FileCopyrightText: 2026 nextcloud-dav contributors
 * SPDX-License-Identifier: AGPL-3.0-or-later
 */

namespace OCA\NextcloudDav\AppInfo;

use OCP\AppFramework\App;
use OCP\AppFramework\Bootstrap\IBootContext;
use OCP\AppFramework\Bootstrap\IBootstrap;
use OCP\AppFramework\Bootstrap\IRegistrationContext;

/**
 * Companion app for the nextcloud-dav sidecar.
 *
 * The migration steps under `lib/Migration/` are discovered automatically by
 * `OC\DB\MigrationService` (file name must match `Version*.php`); nothing has
 * to be registered for them here. The outbox janitor is registered through
 * `<background-jobs>` and the worker command through `<commands>` in
 * `appinfo/info.xml`.
 */
class Application extends App implements IBootstrap {
	public const APP_ID = 'nextcloud_dav';

	/** config.php key holding the shared configuration of the sidecar + this app */
	public const CONFIG_KEY = 'nextcloud_dav';

	/** config.php sub-key for the event dispatch settings */
	public const CONFIG_DISPATCH_KEY = 'event_dispatch';

	/** Table that holds the outbox rows (without the configured db prefix). */
	public const OUTBOX_TABLE = 'dav_event_outbox';

	public function __construct(array $urlParams = []) {
		parent::__construct(self::APP_ID, $urlParams);
	}

	#[\Override]
	public function register(IRegistrationContext $context): void {
	}

	#[\Override]
	public function boot(IBootContext $context): void {
	}
}