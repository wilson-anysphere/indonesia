package com.example;

public class CarMapperImpl implements CarMapper {

    @Override
    public void updateCarDto(Car car, CarDto carDto) {
        if (car == null) {
            return;
        }
        carDto.setSeatCount(car.getNumberOfSeats());
    }
}

