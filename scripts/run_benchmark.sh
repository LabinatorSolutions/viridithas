#!/bin/bash

# name of the cargo package (substitute '-' with '_'!)
NAME=viridithas

# run the command given in the arguments and collect performance statistics
perf record --call-graph dwarf target/release/viridithas bench

# process the performance statistics:
# map the first occurrence of either `even` or `odd` in a flame to `eveod`, and drop the rest
perf script | inferno-collapse-perf | sed \
  -e 's/viridithas:://g' \
  -e 's/;search::<impl chess::board::Board>::alpha_beta/;search::<impl chess::board::Board>::absearch/' \
  -e 's/;search::<impl chess::board::Board>::quiescence//g' \
  -e 's/;search::<impl chess::board::Board>::alpha_beta//g' \
  -e 's/<impl chess::board::Board>/Board/g' > stacks.folded

# create the flamegraph
cat stacks.folded | inferno-flamegraph > flamegraph.svg
