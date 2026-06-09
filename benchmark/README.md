Metric                                 Value
Full run, 1 000 models                 12.9 s (~13 ms/model)
dbt run --select X (per model)         721 ms ± 2 ms
dbt-daemon run --select X (per model)  658 ms ± 58 ms
Daemon speedup                         1.10×
