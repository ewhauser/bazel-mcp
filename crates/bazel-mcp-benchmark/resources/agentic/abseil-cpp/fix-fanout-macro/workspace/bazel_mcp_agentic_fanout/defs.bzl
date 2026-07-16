"""Macros for the fan-out compilation benchmark fixture."""

load("@rules_cc//cc:cc_binary.bzl", "cc_binary")
load("@rules_cc//cc:cc_library.bzl", "cc_library")

def fanout_suite(name, count):
    libraries = []
    for index in range(count):
        unit_name = "%s_unit_%d" % (name, index)
        cc_library(
            name = unit_name,
            srcs = ["unit.cc"],
        )
        libraries.append(":" + unit_name)

    cc_binary(
        name = name,
        srcs = ["main.cc"],
        deps = libraries,
    )
