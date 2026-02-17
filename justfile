
_default:
    just --list

rebuild:
    (cd ./interface && bun run build)
    cargo install --path . --locked --offline
    -spacebot stop
    sleep 1
    spacebot start
    sleep 1
    spacebot status

after-git-pull:
    cargo fetch --locked
