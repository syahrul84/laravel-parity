<?php

// Routes CI adds to a fresh laravel/laravel app, so the runner has something to replay on every Laravel version.

use Illuminate\Http\Request;
use Illuminate\Support\Facades\Route;

Route::get('/parity/ping', function () {
    return response()->json(['ok' => true, 'laravel' => app()->version()]);
});

Route::get('/parity/validate', function (Request $request) {
    $request->validate(['name' => 'required|string|max:20']);

    return response()->json(['name' => $request->input('name')]);
});
