key_store := "key.kjs"
key_alias := "key_test"

clean:
    docker run --rm -it -v "$(pwd)/:/src" \
        -v gradle-cache:/root/.gradle \
        -w /src/android \
        --entrypoint bash sergioribera/rust-android:1.96-sdk-37.0 \
        -c './gradlew clean --no-daemon'

genkey:
    # Run keytool to generate key
    docker run --rm -it -v "$(pwd)/:/src" -w /src --entrypoint keytool sergioribera/rust-android:1.96-sdk-37.0 \
        -genkey -v -keystore {{key_store}} -alias {{key_alias}} -keyalg RSA -keysize 2048 -validity 10000

build:
    # The image ships Gradle 8.2, but AGP 8.13 needs Gradle 8.13+
    # (BouncyCastle 1.79's jar metadata trips older Gradles with
    # "Failed to create Jar file ... bcprov-jdk18on-1.79.jar"). We
    # override the entrypoint and run the in-repo wrapper which
    # downloads the pinned distribution (gradle-8.13-bin) on first
    # use. --no-daemon avoids leaving a long-lived JVM in the
    # ephemeral container.
    #
    # Cache mount avoids re-downloading Gradle 8.13 (~100 MB) and the
    # full dependency tree on every build. The `gradle-cache` named
    # volume is auto-created by docker on first run.
    docker run --rm -it -v "$(pwd)/:/src" \
        -v gradle-cache:/root/.gradle \
        -w /src/android \
        --entrypoint bash sergioribera/rust-android:1.96-sdk-37.0 \
        -c './gradlew assembleRelease --no-daemon'

sign:
    # AGP pre-signs the release APK with the debug keystore (see
    # `app/build.gradle.kts`) so CI artifacts ship installable
    # without provisioning a release keystore. Locally we override
    # that signature with `{{key_store}}` — apksigner replaces the
    # existing v1/v2/v3 signatures in place.
    docker run --rm -it -v "$(pwd)/:/src" -w /src --entrypoint apksigner sergioribera/rust-android:1.96-sdk-37.0 \
        sign --ks-key-alias {{key_alias}} --ks {{key_store}} android/app/build/outputs/apk/release/app-release.apk
    sudo cp android/app/build/outputs/apk/release/app-release.apk \
        android/app/build/outputs/apk/release/app-release-signed.apk

install:
    adb install android/app/build/outputs/apk/release/app-release-signed.apk

run: (clean) (build) (sign) (install)

# Cut a release. Usage: just publish <bump>
#
# `<bump>` is one of:
#   major     1.2.3       -> 2.0.0
#   minor     1.2.3       -> 1.3.0
#   patch     1.2.3       -> 1.2.4
#   rc        1.2.3       -> 1.2.4-rc.1
#             1.2.4-rc.1  -> 1.2.4-rc.2
#             1.2.4-beta.3-> 1.2.4-rc.1  (stream switch)
#   beta      same shape, with -beta.N
#   alpha     same shape, with -alpha.N
#   release   1.2.4-rc.2  -> 1.2.4       (strip pre-release)
#
# Bumps `[workspace.package].version` in the root `Cargo.toml`. Only
# the binaries (`ansyncd`, `ansyncctl`) inherit it via
# `version.workspace = true`; every `crates/*/Cargo.toml` pins its own
# `version = "0.1.0"` and stays put so a release bump doesn't churn
# every library's version, keeping nix store paths (and therefore the
# crane / cache.sergioribera.rs cache) warm across releases.
#
# Refreshes the lockfile workspace-member entries, commits, pushes,
# tags + pushes the tag. `release.yml` fires on the tag and builds
# host bundles + the companion APK so `CARGO_PKG_VERSION` (binaries)
# and `versionName` (APK) line up.
publish bump:
    #!/usr/bin/env bash
    set -euo pipefail

    case "{{bump}}" in
        major|minor|patch|release|rc|beta|alpha) ;;
        *)
            echo "error: bump must be major|minor|patch|release|rc|beta|alpha" >&2
            exit 1
            ;;
    esac

    if [ -n "$(git status --porcelain)" ]; then
        echo "error: working tree is dirty; commit or stash first" >&2
        exit 1
    fi

    # Pull the version out of the [workspace.package] block. Pure awk
    # so the recipe stays free of jq / cargo metadata dependencies.
    current=$(awk '
        /^\[workspace\.package\]/{f=1; next}
        f && /^\[/{exit}
        f && /^version[[:space:]]*=/{
            gsub(/[" ]/, "")
            split($0, a, "=")
            print a[2]
            exit
        }
    ' Cargo.toml)

    if [ -z "${current:-}" ]; then
        echo "error: could not read version from [workspace.package]" >&2
        exit 1
    fi
    if ! echo "$current" | grep -Eq '^[0-9]+\.[0-9]+\.[0-9]+(-[A-Za-z0-9.]+)?$'; then
        echo "error: current version '$current' is not semver" >&2
        exit 1
    fi

    kind="{{bump}}"
    case "$kind" in
        major)
            base=${current%%-*}
            IFS=. read -r maj _min _pat <<<"$base"
            new="$((maj+1)).0.0"
            ;;
        minor)
            base=${current%%-*}
            IFS=. read -r maj min _pat <<<"$base"
            new="${maj}.$((min+1)).0"
            ;;
        patch)
            base=${current%%-*}
            IFS=. read -r maj min pat <<<"$base"
            new="${maj}.${min}.$((pat+1))"
            ;;
        release)
            if [[ "$current" != *-* ]]; then
                echo "error: '$current' is already a stable release" >&2
                exit 1
            fi
            new=${current%%-*}
            ;;
        rc|beta|alpha)
            if [[ "$current" =~ ^([0-9]+\.[0-9]+\.[0-9]+)-${kind}\.([0-9]+)$ ]]; then
                # Same stream: bump the counter.
                new="${BASH_REMATCH[1]}-${kind}.$((BASH_REMATCH[2]+1))"
            elif [[ "$current" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
                # Stable -> first pre-release of next patch.
                IFS=. read -r maj min pat <<<"$current"
                new="${maj}.${min}.$((pat+1))-${kind}.1"
            elif [[ "$current" =~ ^([0-9]+\.[0-9]+\.[0-9]+)-(rc|beta|alpha)\.[0-9]+$ ]]; then
                # Different pre-release stream — keep base, reset to .1.
                new="${BASH_REMATCH[1]}-${kind}.1"
            else
                echo "error: cannot bump ${kind} from '$current'" >&2
                exit 1
            fi
            ;;
    esac

    if git rev-parse "v${new}" >/dev/null 2>&1; then
        echo "error: tag v${new} already exists" >&2
        exit 1
    fi

    echo "bumping: ${current} -> ${new}"
    # Patch only the [workspace.package] block so per-dep `version = ...`
    # entries downstream stay untouched.
    sed -i '/^\[workspace\.package\]/,/^\[/{ s/^version = .*/version = "'"$new"'"/ }' Cargo.toml
    # Refresh lockfile workspace-member entries (skips dep churn).
    cargo update --workspace
    git add Cargo.toml Cargo.lock
    git commit -m "chore(release): ${new}"
    git push origin HEAD
    git tag "v${new}"
    git push origin "v${new}"
    echo "pushed v${new} — release.yml will build bundles + APK"
