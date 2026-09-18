# Benchmarks

`bench_native_vs_php.py` measures the sidecar against Nextcloud's PHP DAV
backend for the same requests, interleaved.

It addresses the two backends **directly**, each through its own base URL:

```sh
# production: PHP is forced with the sidecar's own 501 (nginx replays ?export=1)
python3 bench_native_vs_php.py --user <u> --password-file /tmp/pw \
  --sidecar-base https://cloud.example --php-base https://cloud.example \
  --php-query export=1 --reps 10

# local: sidecar and Nextcloud each on their own port
python3 bench_native_vs_php.py --user alice --password-file <harness>/state/app_password \
  --sidecar-base http://127.0.0.1:17870 --php-base http://127.0.0.1:18081 --reps 15
```

Do **not** try to reach PHP by inserting a second slash in the DAV path
(`users//<uid>/…`): nginx has `merge_slashes on` by default, so it is normalised
back and the sidecar still serves the request. A benchmark built on that trick
compares the sidecar with itself and reports ratios of ~1.0.

Results and interpretation: `docs/BENCHMARKS.md`.
