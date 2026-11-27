The Magic Containers chart fixtures are sanitized, representative
reconstructions of the shape of a live response from
`GET /mc/apps/{id}/statistics/charts`. All identifiers and samples were
made up for the tests. November 1 is the full UTC day being priced.

The four November 1 hourly buckets represent 320, 160, 80 and 80
samples per pod for RAM and CPU. The daily bucket uses those counts as
weights. It reports 250 MiB in bytes and 0.984375 cores, compared with
unweighted hourly means of 264 MiB and 1.0625 cores. Daily traffic is
the sum of those four buckets. October 31 and the partial November 2
buckets exercise client-side window filtering. The extra hour at 01:00
also falls beyond the requested end. The volume chart series are
unverified sample values kept to exercise their exclusion from pricing.
