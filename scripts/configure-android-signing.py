#!/usr/bin/env python3
from pathlib import Path
import sys

path = Path("src-tauri/gen/android/app/build.gradle.kts")
if not path.exists():
    raise SystemExit(f"Android Gradle file not found: {path}")

text = path.read_text(encoding="utf-8")

signing_block = '''    signingConfigs {
        create("release") {
            val keystorePropertiesFile = rootProject.file("keystore.properties")
            val keystoreProperties = Properties().apply {
                if (keystorePropertiesFile.exists()) {
                    keystorePropertiesFile.inputStream().use { load(it) }
                }
            }

            keyAlias = keystoreProperties.getProperty("keyAlias")
            keyPassword = keystoreProperties.getProperty("password")
            storeFile = file(keystoreProperties.getProperty("storeFile"))
            storePassword = keystoreProperties.getProperty("password")
            storeType = keystoreProperties.getProperty("storeType", "PKCS12")
        }
    }

'''

if 'create("release")' not in text:
    marker = "    buildTypes {\n"
    if marker not in text:
        raise SystemExit("Could not locate Android buildTypes block")
    text = text.replace(marker, signing_block + marker, 1)

release_marker = '        getByName("release") {\n'
signing_line = '            signingConfig = signingConfigs.getByName("release")\n'
if signing_line not in text:
    if release_marker not in text:
        raise SystemExit("Could not locate Android release build type")
    text = text.replace(release_marker, release_marker + signing_line, 1)

path.write_text(text, encoding="utf-8")
print("Android release signing configuration applied.")
