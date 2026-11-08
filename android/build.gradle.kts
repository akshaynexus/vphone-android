plugins {
    id("com.android.application") version "9.5.0" apply false
}

// All build output stays under android/build, which is ignored.
allprojects {
    layout.buildDirectory.set(rootDir.resolve("build/gradle/${project.name}"))
}
