<?php

declare(strict_types=1);

/**
 * SPDX-FileCopyrightText: 2026 nextcloud-dav contributors
 * SPDX-License-Identifier: AGPL-3.0-or-later
 */

namespace OCA\NextcloudDav\Outbox;

use OC\DB\ConnectionFactory;
use OCP\Server;
use Psr\Log\LoggerInterface;
use Throwable;

/**
 * Dedicated PostgreSQL LISTEN connection for the outbox wake-up fast path.
 *
 * The sidecar runs `SELECT pg_notify(<channel>, seq)` inside the card-write
 * transaction, so the notification is delivered on commit. This class holds a
 * *separate* `pgsql` connection (not the pooled Doctrine connection used for
 * claiming and dispatching) and blocks in `stream_select()` on its socket with
 * the configured idle poll as a timeout.
 *
 * If the `pgsql` extension, `pg_socket()` or the LISTEN itself is unavailable,
 * {@see isAvailable()} stays false and the caller falls back to plain polling.
 * The fallback is always surfaced to the operator, never silent.
 *
 * The notification payload is only a latency hint; correctness comes from the
 * outbox table, so a missed notification merely costs one poll interval.
 */
final class PostgresListener {
	/** @var \PgSql\Connection|resource|null */
	private mixed $connection = null;

	/** @var resource|null */
	private mixed $socket = null;

	private bool $available = false;

	private string $unavailableReason = '';

	public function __construct(
		private readonly string $channel,
		private readonly LoggerInterface $logger,
	) {
	}

	/**
	 * Open the dedicated connection and start LISTEN.
	 *
	 * @return bool true when the fast path is active
	 */
	public function connect(): bool {
		if (!function_exists('pg_connect') || !function_exists('pg_socket') || !function_exists('pg_get_notify')) {
			return $this->fail('the pgsql extension / pg_socket() is not available');
		}

		try {
			$params = Server::get(ConnectionFactory::class)->createConnectionParams();
		} catch (Throwable $e) {
			return $this->fail('could not read the database connection parameters: ' . $e->getMessage());
		}

		if (($params['driver'] ?? '') !== 'pdo_pgsql') {
			return $this->fail('the configured database is not PostgreSQL');
		}

		$conninfo = $this->buildConninfo($params);
		$connection = @pg_connect($conninfo, PGSQL_CONNECT_FORCE_NEW);
		if ($connection === false) {
			return $this->fail('pg_connect() failed: ' . $this->lastError());
		}

		$result = @pg_query($connection, 'LISTEN "' . str_replace('"', '""', $this->channel) . '"');
		if ($result === false) {
			$error = pg_last_error($connection);
			pg_close($connection);
			return $this->fail('LISTEN "' . $this->channel . '" failed: ' . $error);
		}

		$socket = @pg_socket($connection);
		if ($socket === false) {
			pg_close($connection);
			return $this->fail('pg_socket() returned no socket');
		}

		$this->connection = $connection;
		$this->socket = $socket;
		$this->available = true;
		$this->unavailableReason = '';

		return true;
	}

	/**
	 * Wait for a notification or until the timeout expires.
	 *
	 * Returns true when a notification (or a socket error) was observed, in
	 * which case the caller should claim immediately; false on a clean timeout
	 * or when the fast path is unavailable.
	 */
	public function wait(int $timeoutMs): bool {
		if (!$this->available || $this->connection === null || $this->socket === null) {
			if ($timeoutMs > 0) {
				usleep($timeoutMs * 1000);
			}
			return false;
		}

		if (pg_connection_status($this->connection) !== PGSQL_CONNECTION_OK) {
			$this->markUnavailable('the LISTEN connection was lost');
			return false;
		}

		$read = [$this->socket];
		$write = null;
		$except = null;
		$seconds = intdiv($timeoutMs, 1000);
		$microseconds = ($timeoutMs % 1000) * 1000;

		$ready = @stream_select($read, $write, $except, $seconds, $microseconds);
		if ($ready === false) {
			// Interrupted (e.g. SIGTERM) or the underlying stream failed. The
			// caller checks the stop flag right after; on a genuine stream
			// failure pg_connection_status() catches it on the next round.
			return false;
		}
		if ($ready === 0) {
			return false;
		}

		if (pg_connection_status($this->connection) !== PGSQL_CONNECTION_OK) {
			$this->markUnavailable('the LISTEN connection was lost');
			return false;
		}

		// Consume pending input and drain queued notifications. PQnotifies()
		// (pg_get_notify) is non-blocking and returns false when the queue is
		// empty, so this loop cannot hang.
		@pg_consume_input($this->connection);
		$drained = 0;
		while (@pg_get_notify($this->connection, PGSQL_ASSOC) !== false) {
			$drained++;
			if ($drained >= 1000) {
				break;
			}
		}

		return true;
	}

	public function isAvailable(): bool {
		return $this->available;
	}

	public function getUnavailableReason(): string {
		return $this->unavailableReason;
	}

	public function getChannel(): string {
		return $this->channel;
	}

	public function close(): void {
		if ($this->connection !== null) {
			@pg_close($this->connection);
		}
		$this->connection = null;
		$this->socket = null;
		$this->available = false;
	}

	private function fail(string $reason): bool {
		$this->markUnavailable($reason);
		return false;
	}

	private function markUnavailable(string $reason): void {
		$this->available = false;
		$this->unavailableReason = $reason;
		$this->logger->warning('nextcloud_dav: PostgreSQL LISTEN fast path unavailable: {reason}', [
			'app' => 'nextcloud_dav',
			'reason' => $reason,
		]);
	}

	private function lastError(): string {
		$error = function_exists('pg_last_error') ? @pg_last_error() : '';
		return $error !== false && $error !== '' ? $error : 'unknown error';
	}

	/**
	 * @param array<string, mixed> $params
	 */
	private function buildConninfo(array $params): string {
		$parts = [];
		if (($params['unix_socket'] ?? '') !== '') {
			$parts[] = 'host=' . self::quoteValue((string)$params['unix_socket']);
		} elseif (($params['host'] ?? '') !== '') {
			$parts[] = 'host=' . self::quoteValue((string)$params['host']);
		}
		if (($params['port'] ?? '') !== '' && ($params['port'] ?? '') !== null) {
			$parts[] = 'port=' . (int)$params['port'];
		}
		if (($params['dbname'] ?? '') !== '') {
			$parts[] = 'dbname=' . self::quoteValue((string)$params['dbname']);
		}
		if (($params['user'] ?? '') !== '') {
			$parts[] = 'user=' . self::quoteValue((string)$params['user']);
		}
		if (($params['password'] ?? '') !== '') {
			$parts[] = 'password=' . self::quoteValue((string)$params['password']);
		}
		$parts[] = 'connect_timeout=5';

		return implode(' ', $parts);
	}

	/**
	 * Quote a value for a libpq keyword/value connection string. The value is
	 * never logged.
	 */
	private static function quoteValue(string $value): string {
		return "'" . str_replace(['\\', "'"], ['\\\\', "\\'"], $value) . "'";
	}
}