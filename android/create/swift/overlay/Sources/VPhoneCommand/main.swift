// vphone-android: desktop main.swift plus the environment libcurl needs on Android.
import ArgumentParser
import Foundation

signal(SIGPIPE, SIG_IGN)
vphoneAndroidPrepareEnvironment()
VPhoneCommand.main()
