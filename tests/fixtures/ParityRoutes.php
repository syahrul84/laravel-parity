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

// a PDF export of the kind that broke the next request: under PHP-FPM this setting ends with the request
Route::get('/parity/spoil', function () {
    ini_set('pcre.backtrack_limit', '0');

    return response()->json(['spoiled' => true]);
});

// the app's own code ending the process (a dd() left in a controller does the same)
Route::get('/parity/exit', function () {
    echo 'bye';
    exit;
});
