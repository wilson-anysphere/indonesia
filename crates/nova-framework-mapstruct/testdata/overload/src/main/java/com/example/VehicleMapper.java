package com.example;

import org.mapstruct.Mapper;

@Mapper
public interface VehicleMapper {
    VehicleDto map(Car car);
    VehicleDto map(Bike bike);
}

