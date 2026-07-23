# weathermerge

Merges weather-station CSV files from `data/` into a single `report.md`.

## Run

From the project root:

```sh
python3 main.py
```

The merged report is written to `report.md`.

## Data layout

`data/*.csv` — one file per station network, columns:
`station,date,tmin,tmax,precip`
