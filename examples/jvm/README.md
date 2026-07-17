# JVM reducer examples

This isolated workspace pins `rules_java` and uses Bazel's registered JDK
toolchain. `//:success` is a valid Java binary. The cases exercise real `javac`
symbol diagnostics and a real JVM assertion stack trace without adding a test
framework dependency.
