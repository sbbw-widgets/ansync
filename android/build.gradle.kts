// Root build file — version catalog drives plugin versions.
plugins {
    alias(libs.plugins.android.application) apply false
    alias(libs.plugins.kotlin.android) apply false
    alias(libs.plugins.rust.android) apply false
}
