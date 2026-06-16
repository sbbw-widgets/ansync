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
    # Run apksigner to sign generated apk
    docker run --rm -it -v "$(pwd)/:/src" -w /src --entrypoint apksigner sergioribera/rust-android:1.96-sdk-37.0 \
        sign --ks-key-alias {{key_alias}} --ks {{key_store}} android/app/build/outputs/apk/release/app-release-unsigned.apk
    sudo cp android/app/build/outputs/apk/release/app-release-unsigned.apk \
        android/app/build/outputs/apk/release/app-release-signed.apk

install:
    adb install android/app/build/outputs/apk/release/app-release-signed.apk

run: (build) (sign) (install)
