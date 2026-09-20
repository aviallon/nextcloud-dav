<?php

/**
 * SPDX-FileCopyrightText: 2026 Nextcloud GmbH and Nextcloud contributors
 * SPDX-License-Identifier: AGPL-3.0-or-later
 *
 * Craft session-store fixtures for the session-auth harness. It runs inside the
 * disposable Nextcloud container, talks to the harness redis, and never prints
 * session contents or the passphrase.
 *
 * Usage: php session_craft.php <mode> <in_key> <out_key> [value]
 *   modes: copy | tamper | truncate | set_user_id | strip_dav_flag |
 *          strip_app_password | set_number_app_password | set_number_dav_flag
 */

require '/var/www/html/lib/base.php';

$mode = $argv[1] ?? '';
$inKey = $argv[2] ?? '';
$outKey = $argv[3] ?? '';
$value = $argv[4] ?? '';

$passphrase = getenv('SESSION_PASSPHRASE');
$redisPassword = getenv('REDIS_PASSWORD');
if ($passphrase === false || $inKey === '' || $outKey === '') {
	fwrite(STDERR, "missing arguments\n");
	exit(2);
}

$redis = new Redis();
$redis->connect('redis', 6379);
if ($redisPassword !== false && $redisPassword !== '') {
	$redis->auth($redisPassword);
}

$raw = $redis->get($inKey);
if ($raw === false) {
	fwrite(STDERR, "input session missing\n");
	exit(3);
}

$pattern = '~[0-9a-f]{64,}\|[0-9a-f]{32}\|[0-9a-f]{128}\|3~';
if (!preg_match($pattern, $raw, $match)) {
	fwrite(STDERR, "no authenticated envelope\n");
	exit(4);
}
$blob = $match[0];

$crypto = \OC::$server->get(\OCP\Security\ICrypto::class);

switch ($mode) {
	case 'copy':
		$out = $raw;
		break;
	case 'tamper':
		$out = preg_replace_callback($pattern, static function (array $m): string {
			$s = $m[0];
			$s[0] = ($s[0] === '7') ? '8' : '7';
			return $s;
		}, $raw, 1);
		break;
	case 'truncate':
		$out = preg_replace_callback($pattern, static function (array $m): string {
			return substr($m[0], 0, max(0, strlen($m[0]) - 40));
		}, $raw, 1);
		break;
	case 'set_user_id':
	case 'strip_dav_flag':
	case 'strip_app_password':
	case 'set_number_app_password':
	case 'set_number_dav_flag':
		$json = $crypto->decrypt($blob, urldecode($passphrase));
		$data = json_decode($json, true);
		if (!is_array($data)) {
			fwrite(STDERR, "session payload is not a JSON object\n");
			exit(5);
		}
		if ($mode === 'set_user_id') {
			$data['user_id'] = $value;
		} elseif ($mode === 'strip_dav_flag') {
			unset($data['AUTHENTICATED_TO_DAV_BACKEND']);
		} elseif ($mode === 'strip_app_password') {
			unset($data['app_password']);
		} elseif ($mode === 'set_number_app_password') {
			$data['app_password'] = 1234567890123456789012345678;
		} elseif ($mode === 'set_number_dav_flag') {
			$data['AUTHENTICATED_TO_DAV_BACKEND'] = 42;
		}
		$newBlob = $crypto->encrypt(json_encode($data), urldecode($passphrase));
		$out = str_replace($blob, $newBlob, $raw);
		break;
	default:
		fwrite(STDERR, "unknown mode\n");
		exit(6);
}

$redis->set($outKey, $out);
echo "ok\n";
