from datetime import datetime
from flask import Flask
from flask_apscheduler import APScheduler
import json
import os
import requests


app = Flask(__name__)
scheduler = APScheduler()
scheduler.init_app(app)
scheduler.start()

weather = {}


# interval example
@scheduler.task('interval', id='update weather', minutes=15)
def update_weather():
    print('Updating weather')
    payload = {
        'lat': os.getenv('LAT'),
        'lon': os.getenv('LON'),
        'appid': os.getenv('APP_ID'),
        'units': os.getenv('UNITS', default='imperial')}
    r = requests.get("https://api.openweathermap.org/data/2.5/weather?", params=payload)
    if r.status_code != 200:
        print('error updating weather')
    else:
        global weather
        weather = r.text
        print('Weather updated')


@app.route('/')
def show_weather():
    return weather


@app.route('/datetime')
def show_datetime():
    return json.dumps(datetime.now().isoformat())


update_weather()
