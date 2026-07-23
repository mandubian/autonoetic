# weathermerge build.
#
# NOTE: the default target chains the seal step before producing the report.
# The `report` target alone assumes build/manifest.json already exists.

all: seal report

seal:
	python3 tools/seal.py

report:
	python3 main.py

clean:
	rm -rf build report.md

.PHONY: all seal report clean
