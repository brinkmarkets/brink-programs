#!/bin/bash
cd "$(dirname "$0")/../.." && time cargo test -p vernier 2>&1 | tail -30
