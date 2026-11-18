# Data notices

`us_zips.tsv` and `us_states.tsv` are derived from the GeoNames postal code
export for the United States (https://download.geonames.org/export/zip/US.zip),
trimmed to ZIP code, place name, state and coordinates.

`world_places.tsv.gz` (every other country) is built by
`scripts/build_world_places.py` from these GeoNames exports: the postal code
export for all countries (https://download.geonames.org/export/zip/allCountries.zip),
towns of 1,000+ people (https://download.geonames.org/export/dump/cities1000.zip),
`countryInfo.txt` and `admin1CodesASCII.txt`. Very fine postal codes are merged,
company-only codes are left out, and each place keeps its name, state,
coordinates and population.

GeoNames data is licensed under the Creative Commons Attribution 4.0 License
(https://creativecommons.org/licenses/by/4.0/). © GeoNames (https://www.geonames.org/).

`categories.tsv`, the Google Maps business categories offered on the New
search form, comes from Outscraper's public category list.

The interface icons (`templates/partials/icons.svg`, built by
`scripts/build_icons.py`) are from the Hugeicons free set
(https://hugeicons.com), MIT License, © Hugeicons.
